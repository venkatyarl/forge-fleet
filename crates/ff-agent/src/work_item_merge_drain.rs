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
use std::time::Duration;
use tracing::{info, warn};

use crate::project_github_sync::parse_owner_repo;

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

/// One drain pass. Returns 1 if it merged something this tick, else 0.
pub async fn evaluate_merge_queue(pg: &PgPool) -> Result<usize> {
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
            info!(
                pr = %pr_url,
                "merge_drain: mergeability still computing (UNKNOWN) — deferring to next tick"
            );
            return Ok(0);
        }
        PrMergeState::Other => {}
    }

    // Mark that we're watching this PR's CI (idempotent).
    ff_db::pg_mark_merge_ci_running(pg, item.id).await?;

    match pr_ci_state(&pr_url).await {
        CiState::Pending => {
            // Still running — leave it; we'll re-check next tick.
            Ok(0)
        }
        CiState::Failed(reason) => {
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
            // Honor an existing PR review verdict before spending an
            // autonomous review: APPROVED (operator or external reviewer
            // already signed off) skips the fleet's own review entirely, and
            // CHANGES_REQUESTED rejects the item with the reviewer's reason
            // written to `last_error` so a retry attempt sees why. No verdict
            // (or a gh hiccup) falls through to the autonomous review path,
            // unchanged.
            match pr_review_verdict(&pr_url).await {
                PrReviewVerdict::Approved => {
                    info!(
                        pr = %pr_url,
                        work_item = %item.work_item_id,
                        "merge_drain: PR already has an approved review verdict — skipping autonomous review"
                    );
                }
                PrReviewVerdict::ChangesRequested(reason) => {
                    let failure = format!("review verdict changes_requested: {reason}");
                    warn!(
                        pr = %pr_url,
                        work_item = %item.work_item_id,
                        reason = %failure,
                        "merge_drain: PR has a rejecting review verdict — marking work_item failed"
                    );
                    ff_db::pg_mark_merge_failed(pg, item.id, item.work_item_id, &failure).await?;
                    return Ok(0);
                }
                PrReviewVerdict::None => {
                    match run_pr_review(pg, &pr_url, item.work_item_id).await {
                        Ok((true, reason)) => {
                            info!(
                                pr = %pr_url,
                                work_item = %item.work_item_id,
                                %reason,
                                "merge_drain: autonomous review approved"
                            );
                        }
                        Ok((false, reason)) => {
                            let failure = format!("review rejected: {reason}");
                            warn!(
                                pr = %pr_url,
                                work_item = %item.work_item_id,
                                reason = %failure,
                                "merge_drain: autonomous review rejected PR"
                            );
                            ff_db::pg_mark_merge_failed(pg, item.id, item.work_item_id, &failure)
                                .await?;
                            if let Err(e) = gh_pr_comment(
                                &pr_url,
                                &format!("Autonomous review REJECTED: {reason}"),
                            )
                            .await
                            {
                                warn!(pr = %pr_url, error = %e, "merge_drain: failed to comment review rejection");
                            }
                            return Ok(0);
                        }
                        Err(e) => {
                            warn!(
                                pr = %pr_url,
                                work_item = %item.work_item_id,
                                error = %e,
                                "merge_drain: review unavailable, deferring PR for manual review"
                            );
                            return Ok(0);
                        }
                    }
                }
            }
            match gh_merge_squash(&pr_url).await {
                Ok(()) => {
                    ff_db::pg_mark_merge_merged(pg, item.id, item.work_item_id).await?;
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

    let (title, description): (String, Option<String>) =
        sqlx::query_as("SELECT title, description FROM work_items WHERE id = $1")
            .bind(work_item_id)
            .fetch_one(pg)
            .await
            .context("fetch work_item intent for PR review")?;

    let prompt = format!(
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
    );

    review_ladder(pg, pr_url, &prompt).await
}

/// Cost-optimal PR review ladder (operator design 2026-07-20).
///
/// Free local 30B reviews FIRST; paid/scarce reviewers (480B ring, cloud CLIs)
/// are spent ONLY to CONFIRM a 30B APPROVE — i.e. only to bless a likely merge,
/// never to do the initial review and never on a REJECT. A rejected PR costs
/// zero cloud: it fails and rebuilds locally with the reviewer reason as
/// context (a local coder "fixes it" for free). A weak 30B can false-APPROVE a
/// subtle bug, so approves — not rejects — are the verdict worth a confirmer.
///
/// Ladder:
/// 1. 30B review. REJECT → done (free, rebuild). APPROVE → confirm (step 2).
/// 2. Confirm the approve with the 480B ring if up (stronger, still local);
///    else one cloud CLI; else — no confirmer available — merge on the 30B
///    approve alone (CI is already green and the drain must never freeze).
/// If NO local model is even reachable, fall back to the 480B→cloud path so a
/// review still happens.
async fn review_ladder(pg: &PgPool, pr_url: &str, prompt: &str) -> Result<(bool, String)> {
    let (local_ok, local_reason, local_model) = match local_pool_review(pg, prompt).await {
        Ok(v) => v,
        Err(e) => {
            warn!(
                pr = %pr_url,
                error = %e,
                "merge_drain: no local reviewer — falling back to 480b/cloud review"
            );
            return match review_via_480b(pg, prompt).await {
                Ok((approved, reason)) => Ok((approved, format!("480b: {reason}"))),
                Err(_) => {
                    let (approved, reason, backend) = cloud_cli_review(pg, prompt)
                        .await
                        .context("cloud PR review (no local reviewer)")?;
                    Ok((approved, format!("{backend}: {reason}")))
                }
            };
        }
    };

    // 30B REJECT: trust it, spend nothing. Item fails → rebuilds locally with
    // this reason as context — the free "local coder fixes it" path.
    if !local_ok {
        return Ok((
            false,
            format!("local:{local_model} rejected: {local_reason}"),
        ));
    }

    // 30B APPROVE: confirm before merging (a weak 30B can miss a subtle bug).
    info!(
        pr = %pr_url,
        model = %local_model,
        "merge_drain: local 30B approved — confirming before merge"
    );
    match review_via_480b(pg, prompt).await {
        Ok((true, r)) => Ok((
            true,
            format!("local:{local_model} approved, 480b confirmed: {r}"),
        )),
        Ok((false, r)) => Ok((
            false,
            format!("local:{local_model} approved but 480b rejected: {r}"),
        )),
        Err(_) => match cloud_cli_review(pg, prompt).await {
            Ok((true, r, backend)) => Ok((
                true,
                format!("local:{local_model} approved, {backend} confirmed: {r}"),
            )),
            Ok((false, r, backend)) => Ok((
                false,
                format!("local:{local_model} approved but {backend} rejected: {r}"),
            )),
            Err(_) => {
                // No confirmer up (480B ring + every cloud CLI down). CI is green
                // and the 30B approved — merge rather than freeze the drain.
                warn!(
                    pr = %pr_url,
                    "merge_drain: no confirmer available — merging on CI-green + 30B approval"
                );
                Ok((
                    true,
                    format!("local:{local_model} approved (no confirmer available; CI green)"),
                ))
            }
        },
    }
}

/// Last-resort PR review on ANY healthy local model (a 30B coder). Used only
/// when the 480B ring AND every cloud CLI are unavailable, so a backend outage
/// can never freeze the merge drain. Routes via `fleet_oneshot` with a coder
/// hint; a short timeout keeps a slow node from stalling the drain.
async fn local_pool_review(pg: &PgPool, prompt: &str) -> Result<(bool, String, String)> {
    let resp = crate::fleet_oneshot::fleet_oneshot(
        pg,
        prompt,
        Some("qwen3-coder"),
        Some(Duration::from_secs(120)),
    )
    .await
    .context("local pool PR review")?;
    record_review_interaction(
        pg,
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
        Some(Duration::from_secs(300)),
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

/// True when the model name that served a call is the 480B ring.
fn served_by_480b(model: &str) -> bool {
    model.to_lowercase().contains(REVIEWER_480B_HINT)
}

/// One cloud CLI review pass — first backend that produces output wins, so a
/// leader node missing one vendor CLI still gets a review. Returns
/// `(approved, reason, backend)`.
async fn cloud_cli_review(pg: &PgPool, prompt: &str) -> Result<(bool, String, String)> {
    let mut last_err: Option<anyhow::Error> = None;
    // claude first: it is the most reliable cloud reviewer here; codex has hung
    // (600s stdin block) and auth-expired fleet-wide, so trying it first froze
    // the whole serial drain (2026-07-20 outage). A 90s per-backend cap means a
    // hung/failing backend loses the race to the next one within seconds instead
    // of stalling every drain tick for ten minutes.
    for backend in ["claude", "codex", "kimi"] {
        match crate::cli_executor::execute_cli(backend, prompt, &[], Some(Duration::from_secs(90)))
            .await
        {
            Ok(res) if res.exit_code == 0 && !res.stdout.trim().is_empty() => {
                let (tin, tout) = crate::llm_attribution::parse_cli_token_counts(&format!(
                    "{}\n{}",
                    res.stdout, res.stderr
                ));
                record_review_interaction(
                    pg,
                    backend,
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
        request_meta: serde_json::json!({ "tokens_estimated": tokens_estimated }),
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

fn parse_review_response(response: &str) -> (bool, String) {
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
    Failed(String),
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
    cmd.args(["pr", "checks", pr_url, "--json", "state"]);
    let out = match cmd.output().await {
        Ok(o) => o,
        Err(e) => return CiState::Failed(format!("gh pr checks spawn: {e}")),
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
        return CiState::Failed(format!("a check is {states:?}"));
    }
    if states
        .iter()
        .any(|s| matches!(s.as_str(), "IN_PROGRESS" | "QUEUED" | "PENDING"))
    {
        return CiState::Pending;
    }
    CiState::Success
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
                    if let Err(e) = evaluate_merge_queue(&pg).await {
                        warn!(error = %e, "work_item_merge_drain tick failed");
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
        PrReviewVerdict, parse_review_response, parse_review_verdict, served_by_480b,
        update_branch_api_path,
    };

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
