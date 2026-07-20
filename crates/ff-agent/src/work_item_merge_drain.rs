//! Pillar 4 — merge-queue drain.
//!
//! Leader-only, serial. Closes the loop: dispatch opens PRs and enqueues
//! `work_item_merge_queue`; this drains it one PR at a time per project — wait
//! for the PR's CI to go green, then `gh pr merge --squash --delete-branch`, and
//! mark the work_item `merged`. CI failure → the entry (and work_item) → `failed`.
//!
//! Serialization (one in-flight merge per project) is enforced by
//! [`ff_db::pg_next_merge_queue_item`], so merges land sequentially even though
//! builds ran in parallel across the fleet.
//!
//! Design: `.forgefleet/plans/DECISION-pillar4-canonical-home.md`.

use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{info, warn};
use uuid::Uuid;

use crate::project_github_sync::parse_owner_repo;

/// Short per-review-backend caps so a dead reviewer cannot stall the serial
/// merge drain on one PR for minutes at a time.
const REVIEW_480B_TIMEOUT: Duration = Duration::from_secs(60);
const REVIEW_LOCAL_POOL_TIMEOUT: Duration = Duration::from_secs(60);
const REVIEW_CLOUD_TIMEOUT: Duration = Duration::from_secs(75);
const REVIEW_LOCAL_FIX_ATTEMPTS: u32 = 2;

/// Build a `gh` invocation with the fleet GitHub token injected as `GH_TOKEN`.
///
/// The merge-drain runs on whichever node is currently leader, and that node's
/// *ambient* `gh` auth (`~/.config/gh`) may be unset or point at a retired
/// account (e.g. `taylor-oclaw`) — relying on it silently breaks every drain
/// call with `gh ... failed: authenticate with gh auth login`, stranding
/// CI-green PRs in the queue. Pulling `github_gh_token` from `fleet_secrets` at
/// call time makes the drain authenticate on ANY leader with no per-node `gh
/// auth` and without writing the token to disk. No secret → ambient-auth
/// fallback (unchanged behaviour).
async fn gh_cmd() -> tokio::process::Command {
    let mut c = tokio::process::Command::new("gh");
    if let Some(token) = crate::fleet_info::fetch_secret("github_gh_token").await {
        c.env("GH_TOKEN", token);
    }
    c
}

/// After this many consecutive ticks stuck on the SAME head PR still computing
/// mergeability (UNKNOWN), stop letting it block the serial queue: poke it
/// (force GitHub to recompute) and rotate it to the back of its project queue so
/// the already-computed, mergeable PRs behind it can drain meanwhile.
const MAX_UNKNOWN_DEFERS: u32 = 3;

/// One drain pass. Returns 1 if it merged something this tick, else 0.
///
/// `unknown_defers` tracks, per head merge-queue entry id, how many consecutive
/// ticks GitHub has left it in the `UNKNOWN` (mergeability-computing) state; it
/// is owned by the drain loop so the count survives across ticks. See
/// [`MAX_UNKNOWN_DEFERS`].
pub async fn evaluate_merge_queue(
    pg: &PgPool,
    unknown_defers: &mut HashMap<Uuid, u32>,
) -> Result<usize> {
    let Some(item) = ff_db::pg_next_merge_queue_item(pg).await? else {
        return Ok(0);
    };
    let Some(pr_url) = item.pr_url.clone().filter(|u| !u.trim().is_empty()) else {
        ff_db::pg_mark_merge_failed(pg, item.id, item.work_item_id, "merge entry has no PR url")
            .await?;
        return Ok(0);
    };

    // Conflict-cascade guard: when several sibling PRs land near-simultaneously,
    // each squash-merge advances main and stales the rest — `gh pr merge` then
    // fails with conflicts and the item is shed (2026-07-18: 20 of 38 wave PRs
    // died this way). DIRTY → close this queue row and reset the item for a
    // fresh rebuild against current main; BEHIND → update the PR branch and let
    // CI re-run before merging, so we never squash a stale head.
    match pr_merge_state(&pr_url).await {
        PrMergeState::Dirty => {
            warn!(
                pr = %pr_url,
                work_item = %item.work_item_id,
                "merge_drain: PR conflicts with advanced main — resetting item for rebuild"
            );
            ff_db::pg_mark_merge_failed(
                pg,
                item.id,
                item.work_item_id,
                "PR conflicted with advanced main — auto-reset for rebuild",
            )
            .await?;
            // Preserve the dispatch lane on the reset: this item already BUILT
            // a PR on whatever lane its attempts selected. Zeroing attempts
            // re-routed cloud-built items onto the weak local lane after every
            // sibling conflict, where hard tasks stall out 3x and die
            // (2026-07-19: the observability batch looped this way all night).
            // Cap at 2 so a conflict never pushes an item to the max-attempts
            // kill threshold by itself.
            sqlx::query(
                "UPDATE work_items \
                    SET status = 'ready', attempts = LEAST(attempts, 2), \
                        last_error = NULL, assigned_computer = NULL \
                  WHERE id = $1",
            )
            .bind(item.work_item_id)
            .execute(pg)
            .await?;
            unknown_defers.remove(&item.id);
            return Ok(0);
        }
        PrMergeState::Behind => match gh_update_pr_branch(&pr_url).await {
            Ok(()) => {
                info!(
                    pr = %pr_url,
                    "merge_drain: PR behind main — branch updated, waiting for fresh CI"
                );
                return Ok(0);
            }
            Err(e) => {
                warn!(
                    pr = %pr_url,
                    error = %e,
                    "merge_drain: update-branch failed — continuing with stale head"
                );
            }
        },
        PrMergeState::Unknown => {
            let defers = unknown_defers.entry(item.id).or_insert(0);
            *defers += 1;
            if *defers >= MAX_UNKNOWN_DEFERS {
                // A single slow-to-compute head PR must NOT block the whole
                // serial queue (2026-07-20: a batch update-branch put many PRs
                // into UNKNOWN at once and the drain stalled behind the head).
                // Poke it (best-effort update-branch forces GitHub to recompute
                // mergeability) and rotate it to the back so computed, mergeable
                // PRs drain meanwhile.
                warn!(
                    pr = %pr_url,
                    defers = *defers,
                    "merge_drain: PR stuck UNKNOWN — poking + rotating to back of queue"
                );
                if let Err(e) = gh_update_pr_branch(&pr_url).await {
                    warn!(pr = %pr_url, error = %e, "merge_drain: poke update-branch failed");
                }
                if let Err(e) = ff_db::pg_defer_merge_queue_item_to_back(pg, item.id).await {
                    warn!(pr = %pr_url, error = %e, "merge_drain: rotate-to-back failed");
                }
                unknown_defers.remove(&item.id);
                return Ok(0);
            }
            info!(
                pr = %pr_url,
                defers = *defers,
                "merge_drain: mergeability still computing (UNKNOWN) — deferring to next tick"
            );
            return Ok(0);
        }
        PrMergeState::Other => {}
    }

    // Past the mergeability gate — this item is computed, so clear any stale
    // UNKNOWN-defer count for it.
    unknown_defers.remove(&item.id);

    // Mark that we're watching this PR's CI (idempotent).
    ff_db::pg_mark_merge_ci_running(pg, item.id).await?;

    match pr_ci_state(&pr_url).await {
        CiState::Pending => {
            // Still running — leave it; we'll re-check next tick.
            Ok(0)
        }
        CiState::Failed { reason, run_ids } => {
            if !run_ids.is_empty() && claim_ci_rerun(pg, item.id).await? {
                if let Err(e) = rerun_failed_ci_jobs(&pr_url, &run_ids).await {
                    release_ci_rerun_claim(pg, item.id).await?;
                    return Err(e);
                }
                info!(
                    pr = %pr_url,
                    runs = ?run_ids,
                    "merge_drain: requested one retry of failed CI jobs"
                );
                // GitHub updates the check rollup asynchronously. Re-evaluate
                // next tick instead of treating this stale failed snapshot as
                // the result of the rerun.
                return Ok(0);
            }
            warn!(pr = %pr_url, %reason, "merge_drain: PR CI failed — marking work_item failed");
            ff_db::pg_mark_merge_failed(pg, item.id, item.work_item_id, &reason).await?;
            Ok(0)
        }
        CiState::Success => {
            // SAFETY GATE: never auto-merge LLM-authored code to main unless the
            // operator has explicitly opted in. Default OFF → the PR is left
            // 'mergeable' (CI-green, awaiting approval); flip
            // `work_item_automerge_mode` on (or merge the PR by hand) to land it.
            if !automerge_enabled(pg).await {
                ff_db::pg_mark_merge_mergeable(pg, item.id).await?;
                info!(
                    pr = %pr_url,
                    "merge_drain: PR CI green — MERGEABLE, awaiting operator approval \
                     (work_item_automerge_mode off)"
                );
                return Ok(0);
            }
            // The folder that built this PR already recorded an approval in
            // the queue. pg_next_merge_queue_item filters out every other
            // verdict, so the leader remains a pure serial train-merger.
            match gh_merge_squash(&pr_url).await {
                Ok(()) => {
                    ff_db::pg_mark_merge_merged(pg, item.id, item.work_item_id).await?;
                    let merger = std::env::var("FORGEFLEET_COMPUTER_NAME")
                        .or_else(|_| std::env::var("HOSTNAME"))
                        .unwrap_or_else(|_| "unknown-leader".to_string());
                    sqlx::query(
                        "INSERT INTO work_item_provenance (work_item_id, merged_by, merged_at) \
                         VALUES ($1, $2, NOW()) \
                         ON CONFLICT (work_item_id) DO UPDATE SET \
                           merged_by = EXCLUDED.merged_by, merged_at = EXCLUDED.merged_at, \
                           updated_at = NOW()",
                    )
                    .bind(item.work_item_id)
                    .bind(merger)
                    .execute(pg)
                    .await?;
                    info!(pr = %pr_url, work_item = %item.work_item_id, "merge_drain: merged");
                    Ok(1)
                }
                Err(e) => {
                    let msg = e.to_string();
                    // GitHub computes mergeability ASYNCHRONOUSLY after each
                    // sibling squash-merge; the DIRTY pre-check can race it
                    // (state still UNKNOWN) and the conflict then surfaces
                    // here as "not mergeable"/"cannot be cleanly created".
                    // That is the same condition as DIRTY — recover the item
                    // (rebuild on current main) instead of shedding it.
                    if msg.contains("not mergeable") || msg.contains("cannot be cleanly created") {
                        warn!(
                            pr = %pr_url,
                            work_item = %item.work_item_id,
                            "merge_drain: merge hit late-detected conflict — resetting item for rebuild"
                        );
                        ff_db::pg_mark_merge_failed(
                            pg,
                            item.id,
                            item.work_item_id,
                            "PR conflicted at merge time (async mergeable race) — auto-reset for rebuild",
                        )
                        .await?;
                        // Same lane-preservation rule as the DIRTY reset above.
                        sqlx::query(
                            "UPDATE work_items \
                                SET status = 'ready', attempts = LEAST(attempts, 2), \
                                    last_error = NULL, assigned_computer = NULL \
                              WHERE id = $1",
                        )
                        .bind(item.work_item_id)
                        .execute(pg)
                        .await?;
                        return Ok(0);
                    }
                    let reason = format!("gh pr merge failed: {e}");
                    warn!(pr = %pr_url, %reason, "merge_drain: merge failed");
                    ff_db::pg_mark_merge_failed(pg, item.id, item.work_item_id, &reason).await?;
                    Ok(0)
                }
            }
        }
    }
}

async fn run_pr_review(
    pg: &PgPool,
    pr_url: &str,
    work_item_id: uuid::Uuid,
) -> anyhow::Result<(bool, String)> {
    let distributed = crate::fleet_info::distributed_review_mode_enabled();
    tracing::debug!(
        pr = %pr_url,
        work_item = %work_item_id,
        distributed_review_mode = distributed,
        "run_pr_review: fleet_secrets.distributed_review_mode read"
    );

    let (title, description): (String, Option<String>) =
        sqlx::query_as("SELECT title, description FROM work_items WHERE id = $1")
            .bind(work_item_id)
            .fetch_one(pg)
            .await
            .context("fetch work_item intent for PR review")?;

    if review_ladder_mode(pg).await != "cost_optimal" {
        let prompt = build_pr_review_prompt(pr_url, &title, description.as_deref()).await?;
        return legacy_review_ladder(pg, pr_url, &prompt).await;
    }
    review_ladder(pg, pr_url, &title, description.as_deref()).await
}

async fn build_pr_review_prompt(
    pr_url: &str,
    title: &str,
    description: Option<&str>,
) -> Result<String> {
    let mut diff_cmd = gh_cmd().await;
    diff_cmd.args(["pr", "diff", pr_url]);
    let diff_out = diff_cmd.output().await.context("spawn gh pr diff")?;
    if !diff_out.status.success() {
        anyhow::bail!(
            "gh pr diff failed: {}",
            String::from_utf8_lossy(&diff_out.stderr)
                .trim()
                .chars()
                .take(500)
                .collect::<String>()
        );
    }
    let diff = truncate_chars(&String::from_utf8_lossy(&diff_out.stdout), 40_000);
    Ok(format!(
        "You are reviewing a pull request opened by an autonomous coding fleet.\n\
         Judge whether the change correctly and cleanly implements the requested work item.\n\n\
         Work item title:\n{title}\n\n\
         Work item description:\n{description}\n\n\
         Requirements for approval:\n\
         - The diff matches the stated intent.\n\
         - The diff introduces no regressions.\n\
         - The diff does NOT DEGRADE existing code, documentation, comments, tests, or behavior; \
           for example, replacing a good detailed doc comment or working logic with something \
           worse, shorter, less clear, or less complete is a rejection.\n\
         - The diff is a real, complete change rather than a placeholder, superficial edit, or \
           partial implementation.\n\n\
         Answer with exactly APPROVE or REJECT on the first line. Put a one-line reason on the \
         next line.\n\n\
         Pull request diff (truncated to 40000 chars if needed):\n```diff\n{diff}\n```",
        description = description.unwrap_or_default(),
    ))
}

/// Review with the primary 480B ring, falling back to another working reviewer.
///
/// A 480B approval stands alone. A rejection is independently confirmed by the
/// local 30B pool or a cloud CLI so one local misjudgement cannot fail a good
/// PR. If the 480B ring is unavailable, the same local-to-cloud fallback keeps
/// the drain moving; exhausting the ladder returns an error for manual review.
async fn legacy_review_ladder(pg: &PgPool, pr_url: &str, prompt: &str) -> Result<(bool, String)> {
    match review_via_480b(pg, prompt).await {
        Ok((true, reason)) => Ok((true, format!("480b: {reason}"))),
        Ok((false, reason_480b)) => {
            info!(
                pr = %pr_url,
                reason = %reason_480b,
                "merge_drain: 480b rejected — confirming with another reviewer before failing"
            );
            let (approved, reason, backend) = fallback_review(pg, prompt)
                .await
                .context("confirm 480b rejection")?;
            if approved {
                Ok((
                    true,
                    format!("{backend} overturned 480b rejection ({reason_480b}): {reason}"),
                ))
            } else {
                Ok((
                    false,
                    format!("480b: {reason_480b}; confirmed by {backend}: {reason}"),
                ))
            }
        }
        Err(e) => {
            warn!(
                pr = %pr_url,
                error = %e,
                "merge_drain: 480b reviewer unavailable — falling back to another reviewer"
            );
            let (approved, reason, backend) = fallback_review(pg, prompt)
                .await
                .context("fallback PR review")?;
            Ok((approved, format!("{backend}: {reason}")))
        }
    }
}

/// Cost-optimal ladder: a strong local approval is final; a weak local
/// approval gets one cloud confirmation. Rejections never spend cloud money:
/// a local coder repairs the PR head and the refreshed diff is reviewed again.
async fn review_ladder(
    pg: &PgPool,
    pr_url: &str,
    title: &str,
    description: Option<&str>,
) -> Result<(bool, String)> {
    let mut fix_attempt = 0;
    loop {
        let prompt = build_pr_review_prompt(pr_url, title, description).await?;
        let (approved, reason, reviewer, strong) = match review_via_480b(pg, &prompt).await {
            Ok((approved, reason)) => (approved, reason, "480b".to_string(), true),
            Err(e) => {
                warn!(pr = %pr_url, error = %e, "merge_drain: 480b unavailable — reviewing with local 30b");
                let (approved, reason, model) = local_pool_review(pg, &prompt).await?;
                (approved, reason, format!("local:{model}"), false)
            }
        };

        let rejection = if approved && strong {
            return Ok((true, format!("{reviewer}: {reason}")));
        } else if approved {
            info!(pr = %pr_url, reviewer = %reviewer, "merge_drain: weak local approval — requesting one cloud confirmation");
            let (confirmed, cloud_reason, backend) = cloud_cli_review(pg, &prompt, "cloud_confirm")
                .await
                .context("cloud confirmation of local approval")?;
            if confirmed {
                return Ok((
                    true,
                    format!("{reviewer}: {reason}; confirmed by {backend}: {cloud_reason}"),
                ));
            }
            format!("{backend} overturned {reviewer} approval: {cloud_reason}")
        } else {
            format!("{reviewer}: {reason}")
        };

        if fix_attempt >= REVIEW_LOCAL_FIX_ATTEMPTS {
            return Ok((
                false,
                format!("local fix budget exhausted after {fix_attempt} attempt(s): {rejection}"),
            ));
        }
        fix_attempt += 1;
        if let Err(e) = local_fix_pr(pg, pr_url, title, description, &rejection, fix_attempt).await
        {
            warn!(pr = %pr_url, attempt = fix_attempt, error = %e, "merge_drain: local PR fix attempt failed");
            if fix_attempt >= REVIEW_LOCAL_FIX_ATTEMPTS {
                return Ok((
                    false,
                    format!(
                        "local fix budget exhausted after {fix_attempt} attempt(s): {rejection}; last fix error: {e}"
                    ),
                ));
            }
        }
    }
}

async fn review_ladder_mode(pg: &PgPool) -> String {
    ff_db::pg_read_gate_value(pg, "review_ladder_mode", "cost_optimal", "cost_optimal")
        .await
        .ok()
        .unwrap_or_else(|| "cost_optimal".to_string())
}

/// Repair the PR head in an isolated checkout. The original builder slot is
/// released when the PR enters the queue and may already contain another
/// task, so it is never safe for the merge drain to mutate that path.
async fn local_fix_pr(
    pg: &PgPool,
    pr_url: &str,
    title: &str,
    description: Option<&str>,
    rejection: &str,
    attempt: u32,
) -> Result<()> {
    let (owner, repo) =
        parse_owner_repo(pr_url).with_context(|| format!("unrecognized PR url: {pr_url}"))?;
    let checkout = std::env::temp_dir().join(format!(
        "forgefleet-review-fix-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let checkout_arg = checkout.to_string_lossy().to_string();
    let repo_slug = format!("{owner}/{repo}");

    let result = async {
        run_fix_command(
            "gh",
            &[
                "repo",
                "clone",
                &repo_slug,
                &checkout_arg,
                "--",
                "--no-tags",
            ],
            None,
        )
        .await
        .context("clone repository for local PR fix")?;
        run_fix_command(
            "gh",
            &["pr", "checkout", pr_url, "--force"],
            Some(&checkout),
        )
        .await
        .context("checkout PR head for local fix")?;

        let task = format!(
            "Fix the current PR branch in place after a reviewer rejection.\n\
             Preserve all correct existing work and make the smallest complete fix.\n\
             Work item: {title}\n\
             Description: {}\n\
             Reviewer rejection: {rejection}",
            description.unwrap_or_default()
        );
        let outcome =
            crate::codegen_apply::codegen_apply(pg, &checkout, &task, Some("qwen3-coder"), 1)
                .await
                .context("local coder PR repair")?;
        record_fix_interaction(pg, &task, &outcome, attempt).await;
        if !outcome.applied {
            anyhow::bail!(
                "local coder did not produce a verified fix: {}",
                outcome
                    .error
                    .unwrap_or_else(|| "no applicable edit".to_string())
            );
        }

        run_fix_command(
            "git",
            &["config", "user.name", "ForgeFleet"],
            Some(&checkout),
        )
        .await?;
        run_fix_command(
            "git",
            &["config", "user.email", "fleet@forgefleet.local"],
            Some(&checkout),
        )
        .await?;
        run_fix_command("git", &["add", "-A"], Some(&checkout)).await?;
        let message = format!("fix: address local review (attempt {attempt})");
        run_fix_command("git", &["commit", "-m", &message], Some(&checkout)).await?;
        run_fix_command("git", &["push", "origin", "HEAD"], Some(&checkout)).await?;
        Ok(())
    }
    .await;

    if let Err(e) = std::fs::remove_dir_all(&checkout) {
        warn!(path = %checkout.display(), error = %e, "merge_drain: failed to clean local-fix checkout");
    }
    result
}

async fn run_fix_command(
    program: &str,
    args: &[&str],
    cwd: Option<&std::path::Path>,
) -> Result<()> {
    let mut command = tokio::process::Command::new(program);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if program == "gh" {
        if let Some(token) = crate::fleet_info::fetch_secret("github_gh_token").await {
            command.env("GH_TOKEN", token);
        }
    }
    let output = command
        .output()
        .await
        .with_context(|| format!("spawn {program}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(500)
                .collect::<String>()
        );
    }
    Ok(())
}

async fn record_fix_interaction(
    pg: &PgPool,
    prompt: &str,
    outcome: &crate::codegen_apply::CodegenOutcome,
    attempt: u32,
) {
    let rec = ff_db::InteractionRecord {
        channel: "merge_drain_review".to_string(),
        request_text: prompt.chars().take(16000).collect(),
        request_meta: serde_json::json!({ "stage": "local_fix", "attempt": attempt }),
        engine: Some("local:qwen3-coder".to_string()),
        response_text: format!(
            "applied={} rounds={} error={}",
            outcome.applied,
            outcome.rounds,
            outcome.error.as_deref().unwrap_or("")
        ),
        cost_usd: 0.0,
        outcome: if outcome.applied {
            "success"
        } else {
            "failure"
        }
        .to_string(),
        ..Default::default()
    };
    if let Err(e) = ff_db::pg_record_interaction(pg, &rec).await {
        warn!(error = %e, "merge_drain: failed to log local-fix interaction (non-fatal)");
    }
}

/// Fallback review ladder after the primary 480B reviewer: try the local 30B
/// pool first, then cloud CLIs. A working local review beats burning the whole
/// tick on cloud backends while a healthy on-fleet coder sits idle.
async fn fallback_review(pg: &PgPool, prompt: &str) -> Result<(bool, String, String)> {
    match local_pool_review(pg, prompt).await {
        Ok((approved, reason, model)) => return Ok((approved, reason, format!("local:{model}"))),
        Err(local_err) => {
            warn!(
                error = %local_err,
                "merge_drain: local pool review unavailable — trying cloud reviewers"
            );
        }
    }
    let (approved, reason, backend) = cloud_cli_review(pg, prompt, "legacy_cloud_review").await?;
    Ok((approved, reason, backend))
}

/// PR review on ANY healthy local model (typically the 30B coder pool).
/// Routes via `fleet_oneshot` with a coder hint; a short timeout keeps a slow
/// node from stalling the drain.
async fn local_pool_review(pg: &PgPool, prompt: &str) -> Result<(bool, String, String)> {
    let resp = crate::fleet_oneshot::fleet_oneshot(
        pg,
        prompt,
        Some("qwen3-coder"),
        Some(REVIEW_LOCAL_POOL_TIMEOUT),
    )
    .await
    .context("local pool PR review")?;
    record_review_interaction(
        pg,
        "local_review_30b",
        &resp.model,
        prompt,
        &resp.text,
        resp.tokens_in,
        resp.tokens_out,
        i32::try_from(resp.latency_ms).ok(),
        Some(resp.worker_name.clone()),
        Some(resp.endpoint.clone()),
    )
    .await;
    let (approved, reason) = parse_review_response(&resp.text);
    Ok((approved, reason, resp.model))
}

/// Substring identifying the primary autonomous reviewer — the qwen3-coder-480b
/// ring (single fleet instance). Used both as the `fleet_oneshot` routing hint
/// and to verify which model actually served the call, because `fleet_oneshot`
/// fails over to OTHER deployments when the hinted one is down — a review from
/// a weaker fallback model must not be mistaken for a 480B verdict.
const REVIEWER_480B_HINT: &str = "480b";

/// Cap 480B review concurrency at 1: the ring is a single instance, and the
/// drain is serial anyway — the gate makes that explicit for any future caller
/// that reviews outside the drain loop.
static REVIEW_480B_GATE: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(1));

/// Primary PR review on the 480B ring. `Err` means the ring is unavailable
/// (routing failed, timed out, or `fleet_oneshot` failed over to some other
/// model) — the caller falls back to the cloud review path.
async fn review_via_480b(pg: &PgPool, prompt: &str) -> Result<(bool, String)> {
    let _permit = REVIEW_480B_GATE
        .acquire()
        .await
        .expect("480b review gate is never closed");
    let resp = crate::fleet_oneshot::fleet_oneshot(
        pg,
        prompt,
        Some(REVIEWER_480B_HINT),
        Some(REVIEW_480B_TIMEOUT),
    )
    .await
    .context("480b PR review")?;
    if !served_by_480b(&resp.model) {
        anyhow::bail!(
            "480b ring unavailable — fleet_oneshot failed over to {} on {}",
            resp.model,
            resp.worker_name
        );
    }
    record_review_interaction(
        pg,
        "local_review_480b",
        &resp.model,
        prompt,
        &resp.text,
        resp.tokens_in,
        resp.tokens_out,
        i32::try_from(resp.latency_ms).ok(),
        Some(resp.worker_name.clone()),
        Some(resp.endpoint.clone()),
    )
    .await;
    Ok(parse_review_response(&resp.text))
}

/// True when the model name that served a call is the 480B ring. `pub(crate)`
/// because the in-place dispatch review makes the same failed-over-to-a-weaker-
/// model check before trusting a verdict as a 480B verdict.
pub(crate) fn served_by_480b(model: &str) -> bool {
    model.to_lowercase().contains(REVIEWER_480B_HINT)
}

/// One cloud CLI review pass — first backend that produces output wins, so a
/// leader node missing one vendor CLI still gets a review. Returns
/// `(approved, reason, backend)`.
async fn cloud_cli_review(
    pg: &PgPool,
    prompt: &str,
    stage: &str,
) -> Result<(bool, String, String)> {
    let mut last_err: Option<anyhow::Error> = None;
    let backends: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT backend FROM computer_backends \
          WHERE installed AND authenticated ORDER BY backend",
    )
    .fetch_all(pg)
    .await
    .context("load authenticated cloud review backends")?;
    for backend in backends {
        let budget = crate::cloud_budget::provider_budget(pg, &backend).await;
        if crate::cloud_budget::is_exhausted(budget.as_ref(), chrono::Utc::now()) {
            warn!(
                backend,
                exhausted_until = ?budget.as_ref().and_then(|row| row.window_exhausted_until),
                "merge_drain: skipping quota-exhausted cloud reviewer"
            );
            continue;
        }
        match crate::cli_executor::execute_cli(&backend, prompt, &[], Some(REVIEW_CLOUD_TIMEOUT))
            .await
        {
            Ok(res) if res.exit_code == 0 && !res.stdout.trim().is_empty() => {
                crate::cloud_budget::record_success(
                    pg,
                    &backend,
                    budget.as_ref().and_then(|row| row.window_exhausted_until),
                )
                .await;
                let (tin, tout) = crate::llm_attribution::parse_cli_token_counts(&format!(
                    "{}\n{}",
                    res.stdout, res.stderr
                ));
                record_review_interaction(
                    pg,
                    stage,
                    &backend,
                    prompt,
                    &res.stdout,
                    tin,
                    tout,
                    i32::try_from(res.duration_ms).ok(),
                    None,
                    Some(format!("ff cli {backend}")),
                )
                .await;
                let (approved, reason) = parse_review_response(&res.stdout);
                return Ok((approved, reason, backend.to_string()));
            }
            Ok(res) => {
                let e = anyhow::anyhow!(
                    "{backend} exited {}: {}",
                    res.exit_code,
                    res.stderr.trim().chars().take(300).collect::<String>()
                );
                warn!(backend, error = %e, "merge_drain: cloud review backend failed — trying next");
                last_err = Some(e);
            }
            Err(e) => {
                warn!(backend, error = %e, "merge_drain: cloud review backend unavailable — trying next");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no cloud CLI review backend available")))
}

/// Best-effort `ff_interactions` row for one autonomous review turn (training
/// data — part of the point of routing review through the fleet). Never fails
/// the drain.
#[allow(clippy::too_many_arguments)]
async fn record_review_interaction(
    pg: &PgPool,
    stage: &str,
    engine: &str,
    prompt: &str,
    response: &str,
    tokens_in: i32,
    tokens_out: i32,
    latency_ms: Option<i32>,
    worker_name: Option<String>,
    endpoint: Option<String>,
) {
    // Canonical engine (cloud CLI name or local:<catalog_id>), flagged chars/4
    // estimate when the caller had no reported counts, and config-driven cost.
    let engine = crate::llm_attribution::engine_label(engine);
    let (tokens_in, tokens_out, tokens_estimated) =
        crate::llm_attribution::tokens_or_estimate(tokens_in, tokens_out, prompt, response);
    let cost_usd = crate::llm_attribution::cost_usd(&engine, tokens_in, tokens_out);
    let rec = ff_db::InteractionRecord {
        channel: "merge_drain_review".to_string(),
        request_text: prompt.chars().take(16000).collect(),
        request_meta: serde_json::json!({
            "stage": stage,
            "tokens_estimated": tokens_estimated
        }),
        engine: Some(engine),
        response_text: response.chars().take(16000).collect(),
        tokens_in,
        tokens_out,
        cost_usd,
        latency_ms,
        outcome: "success".to_string(),
        worker_name,
        endpoint,
        ..Default::default()
    };
    if let Err(e) = ff_db::pg_record_interaction(pg, &rec).await {
        warn!(error = %e, "merge_drain: failed to log review interaction (non-fatal)");
    }
}

/// Parse a reviewer's `APPROVE`/`REJECT` first-line verdict + reason. Shared
/// with the in-place dispatch review, which uses the same response contract.
pub(crate) fn parse_review_response(response: &str) -> (bool, String) {
    let mut first_idx = None;
    let mut first_line = "";
    for (idx, line) in response.lines().enumerate() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            first_idx = Some(idx);
            first_line = trimmed;
            break;
        }
    }

    let Some(idx) = first_idx else {
        return (false, "empty review response".to_string());
    };

    let approved = first_line.to_uppercase().starts_with("APPROVE");
    let reason = response
        .lines()
        .skip(idx + 1)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let reason = if reason.is_empty() {
        first_line.to_string()
    } else {
        reason
    };
    (approved, reason)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Whether the operator has opted into auto-merging work_item PRs to main.
/// Default OFF — the loop opens + queues PRs but a human approves the merge.
async fn automerge_enabled(pg: &PgPool) -> bool {
    matches!(
        ff_db::pg_read_gate_value(pg, "work_item_automerge_mode", "off", "off")
            .await
            .as_deref(),
        Ok("on") | Ok("true") | Ok("1")
    )
}

enum CiState {
    Pending,
    Success,
    Failed { reason: String, run_ids: Vec<u64> },
}

/// GitHub's view of how the PR head relates to its base branch.
enum PrMergeState {
    /// Merge-conflicting with the base (GitHub `DIRTY`).
    Dirty,
    /// No conflicts but behind the base (GitHub `BEHIND`).
    Behind,
    /// GitHub is still computing mergeability (`UNKNOWN`) — happens for a few
    /// seconds after any sibling merges. Defer this tick rather than racing
    /// past the guard into a doomed `gh pr merge`.
    Unknown,
    /// Everything else (CLEAN/BLOCKED/UNSTABLE or a gh/API error) — take no
    /// special action; the normal CI→review→merge path decides.
    Other,
}

/// An existing review verdict on the PR, from GitHub's `reviewDecision`.
#[derive(Debug, PartialEq, Eq)]
enum PrReviewVerdict {
    /// `reviewDecision == APPROVED` — someone already signed off; the drain
    /// skips its own autonomous review.
    Approved,
    /// `reviewDecision == CHANGES_REQUESTED` — the PR was rejected; the string
    /// is the rejecting review's reason (its body), surfaced into the
    /// work_item's `last_error` so a retry has the context.
    ChangesRequested(String),
    /// No verdict yet (`REVIEW_REQUIRED` / empty) — the drain runs its own
    /// autonomous review as before.
    None,
}

/// `gh pr view <url> --json reviewDecision,latestReviews`. Any gh/parse error
/// maps to `None` so a hiccup can never fail a healthy item or block the
/// drain — worst case the drain just runs its own review.
async fn pr_review_verdict(pr_url: &str) -> PrReviewVerdict {
    let mut cmd = gh_cmd().await;
    cmd.args([
        "pr",
        "view",
        pr_url,
        "--json",
        "reviewDecision,latestReviews",
    ]);
    let out = match cmd.output().await {
        Ok(o) if o.status.success() => o,
        _ => return PrReviewVerdict::None,
    };
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .map(|v| parse_review_verdict(&v))
        .unwrap_or(PrReviewVerdict::None)
}

/// Pure mapping of `gh pr view --json reviewDecision,latestReviews` output to
/// a verdict. On CHANGES_REQUESTED the reason is the most recent rejecting
/// review's non-empty body (bounded so it fits `last_error`).
fn parse_review_verdict(v: &serde_json::Value) -> PrReviewVerdict {
    match v.get("reviewDecision").and_then(|d| d.as_str()) {
        Some("APPROVED") => PrReviewVerdict::Approved,
        Some("CHANGES_REQUESTED") => {
            let reason = v
                .get("latestReviews")
                .and_then(|r| r.as_array())
                .and_then(|reviews| {
                    reviews
                        .iter()
                        .rev()
                        .filter(|r| {
                            r.get("state").and_then(|s| s.as_str()) == Some("CHANGES_REQUESTED")
                        })
                        .filter_map(|r| r.get("body").and_then(|b| b.as_str()))
                        .map(str::trim)
                        .find(|body| !body.is_empty())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "no reason given".to_string());
            PrReviewVerdict::ChangesRequested(truncate_chars(&reason, 1000))
        }
        _ => PrReviewVerdict::None,
    }
}

/// `gh pr view <url> --json mergeStateStatus`. Any error maps to `Other` so a
/// gh hiccup can never reset a healthy item or block the drain.
async fn pr_merge_state(pr_url: &str) -> PrMergeState {
    let mut cmd = gh_cmd().await;
    cmd.args(["pr", "view", pr_url, "--json", "mergeStateStatus"]);
    let out = match cmd.output().await {
        Ok(o) if o.status.success() => o,
        _ => return PrMergeState::Other,
    };
    let status = serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| {
            v.get("mergeStateStatus")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    match status.as_str() {
        "DIRTY" => PrMergeState::Dirty,
        "BEHIND" => PrMergeState::Behind,
        "UNKNOWN" => PrMergeState::Unknown,
        _ => PrMergeState::Other,
    }
}

/// Merge current base into the PR branch so CI re-runs against what the
/// squash-merge will actually contain. Uses the REST endpoint directly
/// (`PUT /repos/{owner}/{repo}/pulls/{n}/update-branch`) because the
/// `gh pr update-branch` subcommand only exists in gh >= 2.57 and older
/// fleet nodes silently lack it.
async fn gh_update_pr_branch(pr_url: &str) -> Result<()> {
    let api_path =
        update_branch_api_path(pr_url).with_context(|| format!("unrecognized PR url: {pr_url}"))?;
    let mut cmd = gh_cmd().await;
    cmd.args(["api", "-X", "PUT", &api_path]);
    let out = cmd.output().await.context("spawn gh api update-branch")?;
    if out.status.success() {
        return Ok(());
    }
    anyhow::bail!(
        "gh api update-branch failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// `https://github.com/{owner}/{repo}/pull/{n}` → REST path for update-branch.
fn update_branch_api_path(pr_url: &str) -> Option<String> {
    let rest = pr_url.strip_prefix("https://github.com/")?;
    let mut it = rest.splitn(4, '/');
    match (it.next(), it.next(), it.next(), it.next()) {
        (Some(owner), Some(repo), Some("pull"), Some(num))
            if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) =>
        {
            Some(format!("repos/{owner}/{repo}/pulls/{num}/update-branch"))
        }
        _ => None,
    }
}

/// Inspect a PR's checks via `gh pr checks <url> --json state`. No checks yet
/// (or gh transient error) is treated as Pending so we never merge prematurely.
async fn pr_ci_state(pr_url: &str) -> CiState {
    let mut cmd = gh_cmd().await;
    cmd.args(["pr", "checks", pr_url, "--json", "state,detailsUrl"]);
    let out = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            return CiState::Failed {
                reason: format!("gh pr checks spawn: {e}"),
                run_ids: Vec::new(),
            };
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    // gh exits non-zero when checks are failing OR still pending; rely on the
    // JSON states rather than the exit code.
    let states: Vec<String> = serde_json::from_str::<serde_json::Value>(&stdout)
        .ok()
        .and_then(|v| {
            v.as_array().map(|a| {
                a.iter()
                    .filter_map(|c| c.get("state").and_then(|s| s.as_str()).map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default();

    if states.is_empty() {
        return CiState::Pending; // no checks reported yet
    }
    if states
        .iter()
        .any(|s| matches!(s.as_str(), "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT"))
    {
        let value = serde_json::from_str::<serde_json::Value>(&stdout).unwrap_or_default();
        let mut run_ids = value
            .as_array()
            .into_iter()
            .flatten()
            .filter(|check| {
                check
                    .get("state")
                    .and_then(|state| state.as_str())
                    .is_some_and(|state| {
                        matches!(state, "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT")
                    })
            })
            .filter_map(|check| check.get("detailsUrl").and_then(|url| url.as_str()))
            .filter_map(github_actions_run_id)
            .collect::<Vec<_>>();
        run_ids.sort_unstable();
        run_ids.dedup();
        return CiState::Failed {
            reason: format!("a check is {states:?}"),
            run_ids,
        };
    }
    if states
        .iter()
        .any(|s| matches!(s.as_str(), "IN_PROGRESS" | "QUEUED" | "PENDING"))
    {
        return CiState::Pending;
    }
    CiState::Success
}

fn github_actions_run_id(details_url: &str) -> Option<u64> {
    let marker = "/actions/runs/";
    let rest = details_url.split_once(marker)?.1;
    rest.split('/').next()?.parse().ok()
}

async fn claim_ci_rerun(pg: &PgPool, queue_id: uuid::Uuid) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE work_item_merge_queue SET merge_attempts = 1 \
         WHERE id = $1 AND merge_attempts = 0",
    )
    .bind(queue_id)
    .execute(pg)
    .await?
    .rows_affected()
        == 1)
}

async fn release_ci_rerun_claim(pg: &PgPool, queue_id: uuid::Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE work_item_merge_queue SET merge_attempts = 0 \
         WHERE id = $1 AND merge_attempts = 1",
    )
    .bind(queue_id)
    .execute(pg)
    .await?;
    Ok(())
}

async fn rerun_failed_ci_jobs(pr_url: &str, run_ids: &[u64]) -> Result<()> {
    let (owner, repo) = parse_owner_repo(pr_url)
        .with_context(|| format!("unrecognized GitHub PR url: {pr_url}"))?;
    let repo = format!("{owner}/{repo}");
    for run_id in run_ids {
        let mut cmd = gh_cmd().await;
        cmd.args([
            "run",
            "rerun",
            &run_id.to_string(),
            "--failed",
            "--repo",
            &repo,
        ]);
        let out = cmd.output().await.context("spawn gh run rerun --failed")?;
        if !out.status.success() {
            anyhow::bail!(
                "gh run rerun {run_id} --failed failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}

/// `gh pr merge <url> --squash --delete-branch` (the project policy — always
/// delete the branch; see feedback_pr_merge_delete_branch.md).
///
/// If `gh` reports failure because the branch delete step errored (e.g. a
/// transient GitHub 503), but the PR itself is `MERGED`, we treat the merge as
/// a success and only best-effort clean up the branch. A merged PR must never
/// be marked failed because of cleanup.
async fn gh_merge_squash(pr_url: &str) -> Result<()> {
    let mut cmd = gh_cmd().await;
    cmd.args(["pr", "merge", pr_url, "--squash", "--delete-branch"]);
    let out = cmd.output().await.context("spawn gh pr merge")?;
    if out.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    // The squash may have landed even though the --delete-branch step failed.
    if let Some(state) = gh_pr_view_state(pr_url).await {
        if state.eq_ignore_ascii_case("MERGED") {
            warn!(
                pr = %pr_url,
                stderr = %stderr.trim(),
                "merge_drain: PR merged but branch deletion failed — best-effort cleanup"
            );
            if let Err(e) = gh_delete_branch(pr_url).await {
                warn!(
                    pr = %pr_url,
                    error = %e,
                    "merge_drain: best-effort branch delete failed — leaving for janitor"
                );
            }
            return Ok(());
        }
    }

    anyhow::bail!("{}", stderr.trim().chars().take(500).collect::<String>());
}

/// Fetch `state` from `gh pr view --json state`. Used as a reliability check
/// when `gh pr merge` exits non-zero.
async fn gh_pr_view_state(pr_url: &str) -> Option<String> {
    let mut cmd = gh_cmd().await;
    cmd.args(["pr", "view", pr_url, "--json", "state"]);
    let out = cmd.output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_string))
}

/// Best-effort deletion of a PR's head branch via the GitHub API. Used when
/// `gh pr merge --delete-branch` merged the PR but failed to delete the branch.
async fn gh_delete_branch(pr_url: &str) -> Result<()> {
    let mut cmd = gh_cmd().await;
    cmd.args(["pr", "view", pr_url, "--json", "headRefName"]);
    let out = cmd.output().await.context("spawn gh pr view headRefName")?;
    if !out.status.success() {
        anyhow::bail!(
            "gh pr view headRefName failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let head_ref = serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| {
            v.get("headRefName")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })
        .context("missing headRefName in gh pr view response")?;

    let (owner, repo) =
        parse_owner_repo(pr_url).with_context(|| format!("unrecognized PR url: {pr_url}"))?;
    let path = format!("repos/{owner}/{repo}/git/refs/heads/{head_ref}");

    let mut del = gh_cmd().await;
    del.args(["api", "-X", "DELETE", &path]);
    let out = del.output().await.context("spawn gh api delete branch")?;
    if out.status.success() {
        return Ok(());
    }
    anyhow::bail!(
        "gh api delete branch failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
}

async fn gh_pr_comment(pr_url: &str, body: &str) -> Result<()> {
    let mut cmd = gh_cmd().await;
    cmd.args(["pr", "comment", pr_url, "--body", body]);
    let out = cmd.output().await.context("spawn gh pr comment")?;
    if out.status.success() {
        return Ok(());
    }
    anyhow::bail!(
        "{}",
        String::from_utf8_lossy(&out.stderr)
            .trim()
            .chars()
            .take(500)
            .collect::<String>()
    );
}

/// Reconcile work_items stranded in `in_review` whose PR was merged or closed
/// out-of-band — hand-merged by the operator, or merged by any path other than
/// [`evaluate_merge_queue`] (e.g. a plain `gh pr merge` in a terminal, which
/// touches GitHub but never the DB). Without this, such items sit in `in_review`
/// forever and the queue/ETA never reflects that they actually shipped.
///
/// Bounded (25 rows/pass) and leader-gated by the caller. Best-effort: a `gh`
/// failure for one PR is logged and skipped — it never aborts the pass. Only
/// rows that are still `in_review` at UPDATE time are flipped (guards against a
/// racing drain that already claimed the row).
async fn reconcile_orphaned_reviews(pg: &PgPool) -> Result<usize> {
    let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT id, pr_url FROM work_items \
         WHERE status = 'in_review' AND pr_url IS NOT NULL AND pr_url LIKE '%github.com%' \
         ORDER BY COALESCE(started_at, created_at) ASC LIMIT 25",
    )
    .fetch_all(pg)
    .await
    .context("query in_review work_items for reconcile")?;

    let mut reconciled = 0usize;
    for (id, pr_url) in rows {
        let mut cmd = gh_cmd().await;
        cmd.args(["pr", "view", &pr_url, "--json", "state,mergedAt"]);
        let out = match cmd.output().await {
            Ok(o) if o.status.success() => o,
            Ok(o) => {
                warn!(pr = %pr_url, stderr = %String::from_utf8_lossy(&o.stderr).trim(), "reconcile: gh pr view failed — leaving row");
                continue;
            }
            Err(e) => {
                warn!(pr = %pr_url, error = %e, "reconcile: gh spawn failed");
                continue;
            }
        };
        let v: serde_json::Value = match serde_json::from_slice(&out.stdout) {
            Ok(v) => v,
            Err(e) => {
                warn!(pr = %pr_url, error = %e, "reconcile: gh json parse failed");
                continue;
            }
        };
        let merged = v.get("mergedAt").and_then(|m| m.as_str()).is_some();
        let state = v.get("state").and_then(|s| s.as_str()).unwrap_or("");
        let new_status = if merged {
            "merged"
        } else if state.eq_ignore_ascii_case("CLOSED") {
            "cancelled"
        } else {
            continue; // still OPEN — nothing to reconcile
        };
        let affected = sqlx::query(
            "UPDATE work_items SET status = $2, \
             completed_at = COALESCE(completed_at, NOW()), last_error = NULL \
             WHERE id = $1 AND status = 'in_review'",
        )
        .bind(id)
        .bind(new_status)
        .execute(pg)
        .await
        .context("reconcile update work_item status")?
        .rows_affected();
        if affected > 0 {
            info!(work_item = %id, pr = %pr_url, status = new_status, "reconcile: flipped orphaned in_review");
            reconciled += 1;
        }
    }
    Ok(reconciled)
}

/// Spawn the leader-gated drain loop. Mirrors the scheduler's leader check.
pub fn spawn_work_item_merge_drain(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        let mut tick_n: u64 = 0;
        // Per-head UNKNOWN-defer counts, owned by the loop so they persist across
        // ticks (see MAX_UNKNOWN_DEFERS / evaluate_merge_queue).
        let mut unknown_defers: HashMap<Uuid, u32> = HashMap::new();
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // Leader gate with an authoritative DB fallback. The
                    // in-memory leader_cache defaults to FALSE after a daemon
                    // restart and is only warmed by leader_tick on its interval;
                    // during that cold window the (leader-gated) drain silently
                    // no-ops for minutes — the 2026-07-20 freeze where merges
                    // stuck at 141 with ~99 green PRs waiting while priya was the
                    // continuous DB leader. When the cache says "not leader",
                    // confirm against fleet_leader_state (the durable source of
                    // truth) before skipping. Safe: merges are serialized by
                    // FOR UPDATE SKIP LOCKED, so a DB-confirmed leader can never
                    // double-merge.
                    if !crate::leader_cache::is_current_leader()
                        && !db_confirms_leader(&pg, &worker_name).await
                    {
                        continue;
                    }
                    if let Err(e) = evaluate_merge_queue(&pg, &mut unknown_defers).await {
                        warn!(error = %e, "work_item_merge_drain tick failed");
                    }
                    // Drain-tick heartbeat: a leader-gated loop that goes silent
                    // (e.g. a cold leader-cache after a daemon restart) is
                    // otherwise invisible. Emitting an alive marker while we ARE
                    // draining makes silent-death observable — a gap in these
                    // lines while PRs pile up is the signal (2026-07-20 freeze).
                    if tick_n % 30 == 0 {
                        info!(
                            tick = tick_n,
                            pending_unknown = unknown_defers.len(),
                            "work_item_merge_drain: alive (leader, draining)"
                        );
                    }
                    // Every ~20th tick, sweep for PRs merged/closed out-of-band
                    // (hand-merges) so their work_items don't rot in `in_review`.
                    tick_n = tick_n.wrapping_add(1);
                    if tick_n % 20 == 1 {
                        match reconcile_orphaned_reviews(&pg).await {
                            Ok(n) if n > 0 => info!(count = n, "work_item_merge_drain: reconciled orphaned in_review items"),
                            Ok(_) => {}
                            Err(e) => warn!(error = %e, "work_item_merge_drain reconcile failed"),
                        }
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("work_item_merge_drain loop stopped");
    })
}

/// Authoritative leader check straight from `fleet_leader_state` — the durable
/// singleton that decides leadership. Used as a fallback for the cold-cache
/// window right after a daemon restart (see the leader gate above). A fresh
/// heartbeat (<60s) on our own member row means we ARE the leader regardless of
/// the not-yet-warmed in-memory cache.
async fn db_confirms_leader(pg: &PgPool, worker_name: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM fleet_leader_state \
         WHERE member_name = $1 AND heartbeat_at > NOW() - INTERVAL '60 seconds')",
    )
    .bind(worker_name)
    .fetch_one(pg)
    .await
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        PrReviewVerdict, github_actions_run_id, parse_review_response, parse_review_verdict,
        served_by_480b, update_branch_api_path,
    };

    #[test]
    fn extracts_only_github_actions_run_ids() {
        assert_eq!(
            github_actions_run_id(
                "https://github.com/venkatyarl/forge-fleet/actions/runs/123456/job/789"
            ),
            Some(123456)
        );
        assert_eq!(
            github_actions_run_id("https://example.com/external/check/123456"),
            None
        );
        assert_eq!(
            github_actions_run_id("https://github.com/org/repo/actions/runs/nope"),
            None
        );
    }

    #[test]
    fn pr_url_maps_to_update_branch_api_path() {
        assert_eq!(
            update_branch_api_path("https://github.com/venkatyarl/forge-fleet/pull/930").as_deref(),
            Some("repos/venkatyarl/forge-fleet/pulls/930/update-branch")
        );
    }

    #[test]
    fn served_by_480b_matches_the_ring_only() {
        assert!(served_by_480b("qwen3-coder-480b"));
        assert!(served_by_480b("Qwen3-Coder-480B-A35B"));
        // A fail-over to any other deployment must NOT count as a 480B verdict.
        assert!(!served_by_480b("qwen3-coder-30b"));
        assert!(!served_by_480b("local"));
        assert!(!served_by_480b(""));
    }

    #[test]
    fn parse_review_response_verdicts() {
        let (approved, reason) = parse_review_response("APPROVE\nmatches the work item intent");
        assert!(approved);
        assert_eq!(reason, "matches the work item intent");

        let (approved, reason) = parse_review_response("\nREJECT\nplaceholder-only diff");
        assert!(!approved);
        assert_eq!(reason, "placeholder-only diff");

        let (approved, reason) = parse_review_response("");
        assert!(!approved);
        assert_eq!(reason, "empty review response");
    }

    #[test]
    fn approved_review_verdict_skips_own_review() {
        let v = serde_json::json!({
            "reviewDecision": "APPROVED",
            "latestReviews": [{"state": "APPROVED", "body": "lgtm"}],
        });
        assert_eq!(parse_review_verdict(&v), PrReviewVerdict::Approved);
    }

    #[test]
    fn changes_requested_verdict_carries_the_latest_rejection_reason() {
        let v = serde_json::json!({
            "reviewDecision": "CHANGES_REQUESTED",
            "latestReviews": [
                {"state": "CHANGES_REQUESTED", "body": "older reason"},
                {"state": "APPROVED", "body": "lgtm"},
                {"state": "CHANGES_REQUESTED", "body": "  breaks the scheduler tick  "},
            ],
        });
        assert_eq!(
            parse_review_verdict(&v),
            PrReviewVerdict::ChangesRequested("breaks the scheduler tick".to_string())
        );

        // Empty bodies fall back to a placeholder rather than an empty reason.
        let v = serde_json::json!({
            "reviewDecision": "CHANGES_REQUESTED",
            "latestReviews": [{"state": "CHANGES_REQUESTED", "body": ""}],
        });
        assert_eq!(
            parse_review_verdict(&v),
            PrReviewVerdict::ChangesRequested("no reason given".to_string())
        );
    }

    #[test]
    fn missing_or_pending_review_verdict_runs_own_review() {
        for v in [
            serde_json::json!({}),
            serde_json::json!({"reviewDecision": ""}),
            serde_json::json!({"reviewDecision": "REVIEW_REQUIRED"}),
            serde_json::json!({"reviewDecision": null}),
        ] {
            assert_eq!(parse_review_verdict(&v), PrReviewVerdict::None, "{v}");
        }
    }

    #[test]
    fn malformed_pr_urls_are_rejected() {
        for bad in [
            "https://github.com/venkatyarl/forge-fleet",
            "https://github.com/venkatyarl/forge-fleet/issues/930",
            "https://github.com/venkatyarl/forge-fleet/pull/",
            "https://github.com/venkatyarl/forge-fleet/pull/93x",
            "http://github.com/venkatyarl/forge-fleet/pull/930",
        ] {
            assert!(update_branch_api_path(bad).is_none(), "{bad}");
        }
    }
}
