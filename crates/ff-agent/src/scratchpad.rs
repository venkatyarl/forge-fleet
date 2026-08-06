//! Agent working memory — the "Scratchpad".
//!
//! A small, byte-capped, agent-self-editable text surface with fixed blocks
//! and layered scope. Foreground writes fail closed when they would exceed the
//! cap. The dreamer repairs legacy/lowered-cap scopes by summarizing the
//! lowest-priority block and pushing full pre-summary content into Brain, so
//! nothing is truly lost. Sits *beside* `session_brain`, *above*
//! Brain/Cortex/Vault.
//!
//! ff-db owns the transactional SQL primitives (`pg_memory_*`); this module
//! owns the string-edit ops (`add`/`replace`/`remove`) and the
//! consolidate-and-forget driver (which calls a summarizer LLM).
//!
//! Design: `plans/agent-working-memory.md` (LLM council 2026-06-19).

use anyhow::{Context, Result, bail};
use ff_db::queries::{
    MEMORY_BLOCKS, MemoryBlock, MemoryBlockWriteStatus, MemoryEvictionArchive,
    MemoryTrySetBlockRequest,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{info, warn};

const DEFAULT_USER: &str = "venkat";

/// Eviction priority: `scratch` first, `decisions` last (only ever summarized).
const EVICTION_ORDER: [&str; 5] = ["scratch", "findings", "state", "task", "decisions"];

/// Decisions are never hard-trimmed; all other blocks may be deterministically
/// reduced after their complete previous value is durably archived.
const HARD_TRIM_ORDER: [&str; 4] = ["scratch", "findings", "state", "task"];

/// Max consolidate-and-forget passes before falling back to a hard trim.
const MAX_CONSOLIDATE_PASSES: usize = 5;

/// Four 32-bit byte-sized blocks need at most 31 halvings each. This bound is
/// intentionally generous while remaining deterministic under corrupt data.
const MAX_HARD_TRIM_PASSES: usize = 128;

/// Result of a memory write, mirrored back to the caller / tool response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WriteResult {
    pub scope_type: String,
    pub scope_key: String,
    pub block: String,
    pub bytes_used: i64,
    pub cap_bytes: i32,
    pub consolidated: bool,
    pub over_cap: bool,
    pub data_loss: bool,
    pub status: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ConsolidationResult {
    pub changed: bool,
    pub data_loss: bool,
    pub over_cap: bool,
    mutation_count: u64,
    final_scope_hash: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct TrimResult {
    changed: bool,
    data_loss: bool,
    bytes_used: Option<i64>,
    scope_hash: Option<String>,
}

/// One non-empty block eligible for operator-requested compaction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CompactEligibleBlock {
    pub block: String,
    pub bytes: i32,
    /// Decisions may be summarized, but are deliberately never hard-trimmed.
    pub hard_trim_eligible: bool,
}

/// Preview or outcome of one `ff memory compact` request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MemoryCompactResult {
    pub scope_type: String,
    pub scope_key: String,
    pub before_bytes: i64,
    pub after_bytes: i64,
    pub cap_bytes: i32,
    pub eligible_blocks: Vec<CompactEligibleBlock>,
    pub applied: bool,
    pub changed: bool,
    pub data_loss: bool,
    pub still_over_cap: bool,
    pub eviction_evidence: i64,
    pub brain_evidence: i64,
    pub evictions_created: i64,
    pub brain_candidates_created: i64,
    pub status: String,
}

#[derive(Debug, Clone, Copy)]
enum MemoryEdit<'a> {
    Add(&'a str),
    Replace { old: &'a str, new: &'a str },
    Remove(Option<&'a str>),
}

impl MemoryEdit<'_> {
    fn apply(self, current: &str, block: &str) -> Result<String> {
        match self {
            Self::Add(text) => Ok(if current.is_empty() {
                text.to_string()
            } else {
                format!("{current}\n{text}")
            }),
            Self::Replace { old, new } => {
                if old.is_empty() {
                    bail!("memory_replace: 'old' must be non-empty");
                }
                let matches = current.matches(old).count();
                if matches == 0 {
                    bail!("memory_replace: 'old' not found in block '{block}'");
                }
                if matches > 1 {
                    bail!(
                        "memory_replace: 'old' matches {matches}× in block '{block}' (must be unique)"
                    );
                }
                Ok(current.replacen(old, new, 1))
            }
            Self::Remove(None) => Ok(String::new()),
            Self::Remove(Some(text)) => match current.find(text) {
                Some(index) => {
                    let mut next = current.to_string();
                    next.replace_range(index..index + text.len(), "");
                    Ok(next.replace("\n\n", "\n").trim().to_string())
                }
                None => bail!("memory_remove: text not found in block '{block}'"),
            },
        }
    }
}

fn valid_scope_type(scope_type: &str) -> bool {
    matches!(scope_type, "session" | "agent" | "project")
}

fn valid_block(block: &str) -> bool {
    MEMORY_BLOCKS.contains(&block)
}

fn validate(scope_type: &str, block: &str) -> Result<()> {
    if !valid_scope_type(scope_type) {
        bail!("invalid scope_type '{scope_type}' (want session|agent|project)");
    }
    if !valid_block(block) {
        bail!(
            "invalid block '{block}' (want one of {})",
            MEMORY_BLOCKS.join("|")
        );
    }
    Ok(())
}

/// Read the working set for a scope — all blocks, or a single `block`.
pub async fn memory_get(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    block: Option<&str>,
) -> Result<Vec<MemoryBlock>> {
    if !valid_scope_type(scope_type) {
        bail!("invalid scope_type '{scope_type}' (want session|agent|project)");
    }
    let all = ff_db::queries::pg_memory_get_all(pool, scope_type, scope_key)
        .await
        .context("read working memory")?;
    Ok(match block {
        Some(b) => all.into_iter().filter(|m| m.block == b).collect(),
        None => all,
    })
}

/// Append `text` to a block (newline-separated; creates the block if absent).
pub async fn memory_add(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    block: &str,
    text: &str,
) -> Result<WriteResult> {
    validate(scope_type, block)?;
    write_block(pool, scope_type, scope_key, block, MemoryEdit::Add(text)).await
}

/// Replace the single occurrence of `old` with `new` in a block.
/// Errors unless `old` matches exactly once (avoids ambiguous edits).
pub async fn memory_replace(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    block: &str,
    old: &str,
    new: &str,
) -> Result<WriteResult> {
    validate(scope_type, block)?;
    write_block(
        pool,
        scope_type,
        scope_key,
        block,
        MemoryEdit::Replace { old, new },
    )
    .await
}

/// Remove one occurrence of `text` from a block, or clear the block entirely
/// when `text` is `None`.
pub async fn memory_remove(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    block: &str,
    text: Option<&str>,
) -> Result<WriteResult> {
    validate(scope_type, block)?;
    write_block(pool, scope_type, scope_key, block, MemoryEdit::Remove(text)).await
}

/// Set the per-scope byte cap (`scope_key == ""` sets the scope_type default).
pub async fn memory_set_cap(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    cap_bytes: i32,
) -> Result<()> {
    if !valid_scope_type(scope_type) {
        bail!("invalid scope_type '{scope_type}' (want session|agent|project)");
    }
    ff_db::queries::pg_memory_set_cap(pool, scope_type, scope_key, cap_bytes)
        .await
        .context("set memory cap")
}

/// Preview or explicitly apply compaction for one exact Scratchpad scope.
///
/// The default CLI path calls this with `apply = false` and performs reads
/// only. Apply delegates to the same consolidate-and-forget driver used by the
/// dreamer, then verifies that every successful mutation has durable eviction
/// evidence. Brain candidates are an additional durable copy and are reported
/// separately; the eviction row itself always contains the full pre-mutation
/// content so a Brain outage cannot turn compaction into silent loss.
pub async fn memory_compact(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    apply: bool,
) -> Result<MemoryCompactResult> {
    if !valid_scope_type(scope_type) {
        bail!("invalid scope_type '{scope_type}' (want session|agent|project)");
    }

    let before = ff_db::queries::pg_memory_compact_snapshot(pool, scope_type, scope_key)
        .await
        .context("read memory compact preview")?;
    let before_bytes = before
        .scope
        .blocks
        .iter()
        .map(|block| i64::from(block.bytes))
        .sum();
    let cap_bytes = before.scope.cap_bytes;
    let eligible_blocks = compact_eligible_blocks(&before.scope.blocks);
    let evidence_before = before.evidence;
    let was_over_cap = before_bytes > i64::from(cap_bytes);

    if !apply {
        return Ok(MemoryCompactResult {
            scope_type: scope_type.to_string(),
            scope_key: scope_key.to_string(),
            before_bytes,
            after_bytes: before_bytes,
            cap_bytes,
            eligible_blocks,
            applied: false,
            changed: false,
            data_loss: false,
            still_over_cap: was_over_cap,
            eviction_evidence: evidence_before.evictions,
            brain_evidence: evidence_before.brain_candidates,
            evictions_created: 0,
            brain_candidates_created: 0,
            status: compact_status(false, was_over_cap, false, false, was_over_cap),
        });
    }

    let consolidation = consolidate_and_forget_from(
        pool,
        scope_type,
        scope_key,
        cap_bytes,
        Some(before.scope.scope_hash),
        i64::from(cap_bytes),
    )
    .await?;
    let after = ff_db::queries::pg_memory_compact_snapshot(pool, scope_type, scope_key)
        .await
        .context("verify memory compact result")?;
    let after_bytes = after
        .scope
        .blocks
        .iter()
        .map(|block| i64::from(block.bytes))
        .sum();
    let cap_after = after.scope.cap_bytes;
    let evidence_after = after.evidence;
    let evictions_created = evidence_after.evictions - evidence_before.evictions;
    let brain_candidates_created =
        evidence_after.brain_candidates - evidence_before.brain_candidates;

    if evictions_created < 0 || brain_candidates_created < 0 {
        bail!("Scratchpad preservation evidence changed concurrently; refusing a success result");
    }
    if cap_after != cap_bytes {
        bail!("Scratchpad cap changed concurrently; retry compaction");
    }
    if consolidation.final_scope_hash.as_deref() != Some(after.scope.scope_hash.as_str()) {
        bail!("Scratchpad scope changed concurrently; retry compaction");
    }
    if consolidation.mutation_count > evictions_created as u64 {
        bail!(
            "Scratchpad preservation verification failed: {} mutation(s), but only {evictions_created} durable eviction record(s) were created",
            consolidation.mutation_count
        );
    }
    if !consolidation.changed && after_bytes != before_bytes {
        bail!("Scratchpad scope changed concurrently; retry compaction");
    }
    if after_bytes > before_bytes {
        bail!("Scratchpad scope grew during compaction; refusing a success result");
    }

    let still_over_cap = after_bytes > i64::from(cap_bytes);
    Ok(MemoryCompactResult {
        scope_type: scope_type.to_string(),
        scope_key: scope_key.to_string(),
        before_bytes,
        after_bytes,
        cap_bytes,
        eligible_blocks,
        applied: true,
        changed: consolidation.changed,
        data_loss: consolidation.data_loss,
        still_over_cap,
        eviction_evidence: evidence_after.evictions,
        brain_evidence: evidence_after.brain_candidates,
        evictions_created,
        brain_candidates_created,
        status: compact_status(
            true,
            was_over_cap,
            consolidation.changed,
            consolidation.data_loss,
            still_over_cap,
        ),
    })
}

/// Compare-and-set a block's edited content without ever growing the scope
/// above its cap. An over-cap proposal gets one bounded compaction attempt,
/// followed by one fresh read/recompute/CAS; every other failure is surfaced
/// with the requested edit unapplied.
async fn write_block(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    block: &str,
    edit: MemoryEdit<'_>,
) -> Result<WriteResult> {
    write_block_with_retry_hook(pool, scope_type, scope_key, block, edit, || async {}).await
}

async fn write_block_with_retry_hook<Hook, HookFuture>(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    block: &str,
    edit: MemoryEdit<'_>,
    before_retry: Hook,
) -> Result<WriteResult>
where
    Hook: FnOnce() -> HookFuture + Send,
    HookFuture: std::future::Future<Output = ()> + Send,
{
    let expected_content = ff_db::queries::pg_memory_get_block(pool, scope_type, scope_key, block)
        .await
        .context("read bounded memory block before write")?;
    let content = edit.apply(&expected_content, block)?;
    let write = ff_db::queries::pg_memory_try_set_block(
        pool,
        MemoryTrySetBlockRequest {
            scope_type,
            scope_key,
            block,
            expected_content: &expected_content,
            new_content: &content,
            allow_over_cap_repair: false,
            eviction_archive: None,
        },
    )
    .await
    .context("write bounded memory block")?;
    match write.status {
        MemoryBlockWriteStatus::Applied => {
            return Ok(WriteResult {
                scope_type: scope_type.to_string(),
                scope_key: scope_key.to_string(),
                block: block.to_string(),
                bytes_used: write.bytes_used,
                cap_bytes: write.cap_bytes,
                consolidated: false,
                over_cap: write.over_cap,
                data_loss: false,
                status: write_status(write.over_cap, false, false),
            });
        }
        MemoryBlockWriteStatus::Busy => {
            bail!(
                "Scratchpad scope is being updated concurrently; requested write was not applied; retry"
            )
        }
        MemoryBlockWriteStatus::Stale => {
            bail!(
                "Scratchpad block changed concurrently; requested write was not applied; read it again before retrying"
            )
        }
        MemoryBlockWriteStatus::OverCap => {}
    }

    let expected_scope_hash = write
        .scope_hash
        .as_deref()
        .context("over-cap Scratchpad result omitted its scope revision")?;
    let requested_delta = i64::try_from(content.len())
        .and_then(|new_bytes| i64::try_from(expected_content.len()).map(|old| new_bytes - old))
        .context("Scratchpad edit is too large")?;
    let target_bytes = i64::from(write.cap_bytes)
        .saturating_sub(requested_delta)
        .max(0);
    let consolidation = consolidate_and_forget_from(
        pool,
        scope_type,
        scope_key,
        write.cap_bytes,
        Some(expected_scope_hash.to_string()),
        target_bytes,
    )
    .await
    .context("Scratchpad auto-compaction failed; requested write was not applied")?;

    // Compaction may summarize the target block itself. Re-read it and apply
    // the original semantic edit to the current value rather than replaying a
    // stale full-content replacement.
    let retry_expected = ff_db::queries::pg_memory_get_block(pool, scope_type, scope_key, block)
        .await
        .context("read Scratchpad block after auto-compaction")?;
    let retry_content = edit.apply(&retry_expected, block).with_context(
        || "Scratchpad target changed during auto-compaction; requested write was not applied",
    )?;

    // The test hook deliberately sits after the fresh read/recompute and
    // before the retry CAS, making the stale-writer interleaving deterministic
    // without sleeps or process-global mutable state.
    before_retry().await;

    let retry = ff_db::queries::pg_memory_try_set_block(
        pool,
        MemoryTrySetBlockRequest {
            scope_type,
            scope_key,
            block,
            expected_content: &retry_expected,
            new_content: &retry_content,
            allow_over_cap_repair: false,
            eviction_archive: None,
        },
    )
    .await
    .context("retry bounded memory block after auto-compaction")?;
    match retry.status {
        MemoryBlockWriteStatus::Applied => Ok(WriteResult {
            scope_type: scope_type.to_string(),
            scope_key: scope_key.to_string(),
            block: block.to_string(),
            bytes_used: retry.bytes_used,
            cap_bytes: retry.cap_bytes,
            consolidated: consolidation.changed,
            over_cap: retry.over_cap,
            data_loss: consolidation.data_loss,
            status: write_status(
                retry.over_cap,
                consolidation.changed,
                consolidation.data_loss,
            ),
        }),
        MemoryBlockWriteStatus::Busy => bail!(
            "Scratchpad scope became busy after auto-compaction; requested write was not applied; retry"
        ),
        MemoryBlockWriteStatus::Stale => bail!(
            "Scratchpad block changed after auto-compaction; requested write was not applied; read it again before retrying"
        ),
        MemoryBlockWriteStatus::OverCap => bail!(
            "Scratchpad auto-compaction could not make enough space: requested write would use {}/{} bytes and was not applied; remove or replace existing content first",
            retry.bytes_used,
            retry.cap_bytes
        ),
    }
}

/// Consolidate-and-forget: until the scope reaches the requested byte target,
/// pick the lowest-priority non-empty block, summarize it (preserving decisions
/// / paths / commands / IDs / failures), push the full pre-summary content into
/// Brain, record an eviction row, and replace the block with the summary.
/// Falls back to a hard trim if the summarizer is unavailable.
/// `pub(crate)` for the dreamer's cap re-enforcement sweep.
pub(crate) async fn consolidate_and_forget(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    cap: i32,
) -> Result<ConsolidationResult> {
    consolidate_and_forget_from(pool, scope_type, scope_key, cap, None, i64::from(cap)).await
}

async fn consolidate_and_forget_from(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    cap: i32,
    mut expected_scope_hash: Option<String>,
    target_bytes: i64,
) -> Result<ConsolidationResult> {
    let mut result = ConsolidationResult::default();
    let mut skipped_blocks: Vec<String> = Vec::new();

    for _ in 0..MAX_CONSOLIDATE_PASSES {
        let snapshot = ff_db::queries::pg_memory_scope_snapshot(pool, scope_type, scope_key)
            .await
            .context("read locked Scratchpad scope for consolidation")?;
        if snapshot.cap_bytes != cap {
            bail!("Scratchpad cap changed concurrently; retry consolidation");
        }
        if let Some(expected) = expected_scope_hash.as_deref()
            && snapshot.scope_hash != expected
        {
            bail!("Scratchpad scope changed concurrently; retry consolidation");
        }
        expected_scope_hash = Some(snapshot.scope_hash.clone());
        let blocks = snapshot.blocks;
        let total: i64 = blocks.iter().map(|b| b.bytes as i64).sum();
        if total <= target_bytes {
            result.over_cap = total > i64::from(cap);
            result.final_scope_hash = expected_scope_hash;
            return Ok(result);
        }

        // Pick the next eligible block. A non-shrinking pass is skipped until a
        // later pass makes progress, so one stubborn block cannot spin forever.
        let target = next_eligible_block(&blocks, &skipped_blocks);
        let Some(target) = target else {
            break; // nothing left to evict
        };

        let summary = match summarize_block(pool, &target.block, &target.content).await {
            Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
            Ok(_) | Err(_) => {
                // Summarizer unavailable/empty → hard-trim backstop this pass.
                // Decisions are protected: if they cannot be summarized, leave
                // them intact and let the final result report still_over_cap.
                if !hard_trim_eligible(&target.block) {
                    skipped_blocks.push(target.block.clone());
                    continue;
                }
                let trim = hard_trim(
                    pool,
                    scope_type,
                    scope_key,
                    target,
                    expected_scope_hash.as_deref().expect("snapshot hash"),
                    cap,
                )
                .await?;
                if trim.changed {
                    expected_scope_hash = trim.scope_hash.clone();
                }
                result.changed |= trim.changed;
                result.data_loss |= trim.data_loss;
                result.mutation_count += u64::from(trim.changed);
                let after = trim.bytes_used.unwrap_or(total);
                if after < total {
                    skipped_blocks.clear();
                } else {
                    skipped_blocks.push(target.block.clone());
                }
                continue;
            }
        };

        // Model output is untrusted. Never replace a block with a summary that
        // does not strictly reduce its UTF-8 byte length.
        if !summary_makes_progress(&target.content, &summary) {
            if !hard_trim_eligible(&target.block) {
                skipped_blocks.push(target.block.clone());
                continue;
            }
            let trim = hard_trim(
                pool,
                scope_type,
                scope_key,
                target,
                expected_scope_hash.as_deref().expect("snapshot hash"),
                cap,
            )
            .await?;
            if trim.changed {
                expected_scope_hash = trim.scope_hash.clone();
            }
            result.changed |= trim.changed;
            result.data_loss |= trim.data_loss;
            result.mutation_count += u64::from(trim.changed);
            let after = trim.bytes_used.unwrap_or(total);
            if after < total {
                skipped_blocks.clear();
            } else {
                skipped_blocks.push(target.block.clone());
            }
            continue;
        }

        // Push full pre-summary content down to Brain as an optional second
        // durable copy. The eviction row below is the mandatory archive and
        // therefore also embeds the complete pre-summary content.
        let brain_ref =
            push_to_brain(pool, scope_type, scope_key, &target.block, &target.content).await;

        let replacement = ff_db::queries::pg_memory_try_set_block(
            pool,
            MemoryTrySetBlockRequest {
                scope_type,
                scope_key,
                block: &target.block,
                expected_content: &target.content,
                new_content: &summary,
                allow_over_cap_repair: true,
                eviction_archive: Some(MemoryEvictionArchive {
                    expected_scope_hash: expected_scope_hash.as_deref().expect("snapshot hash"),
                    expected_cap_bytes: cap,
                    prev_bytes: target.bytes,
                    result_summary: &summary,
                    summarizer: "fleet-summarizer",
                    brain_ref: brain_ref.as_deref(),
                }),
            },
        )
        .await
        .context("replace block with bounded summary")?;
        match replacement.status {
            MemoryBlockWriteStatus::Applied => {
                if replacement.eviction_id.is_none() {
                    bail!("Scratchpad summary replacement committed without eviction evidence");
                }
                result.changed = true;
                result.mutation_count += 1;
                expected_scope_hash = Some(
                    replacement
                        .scope_hash
                        .clone()
                        .context("summary replacement omitted its scope revision")?,
                );
            }
            MemoryBlockWriteStatus::Stale => {
                bail!("Scratchpad block changed concurrently; retry consolidation")
            }
            MemoryBlockWriteStatus::Busy => {
                bail!("Scratchpad scope is being updated concurrently; retry consolidation")
            }
            MemoryBlockWriteStatus::OverCap => {
                bail!("Scratchpad rejected a non-reducing consolidation")
            }
        }
        let after = replacement.bytes_used;
        if after < total {
            skipped_blocks.clear();
        } else {
            skipped_blocks.push(target.block.clone());
        }
        info!(
            scope_type, scope_key, block = %target.block,
            prev_bytes = target.bytes, brain = brain_ref.is_some(),
            "scratchpad: consolidated block"
        );
    }

    // Deterministic backstop: repeatedly halve every hard-trimmable block in
    // eviction order until the scope fits. Full pre-trim content is archived
    // before each compare-and-set replacement.
    let mut skipped_blocks = Vec::new();
    for _ in 0..MAX_HARD_TRIM_PASSES {
        let snapshot = ff_db::queries::pg_memory_scope_snapshot(pool, scope_type, scope_key)
            .await
            .context("read locked Scratchpad scope for hard trim")?;
        if snapshot.cap_bytes != cap {
            bail!("Scratchpad cap changed concurrently; retry hard trim");
        }
        if let Some(expected) = expected_scope_hash.as_deref()
            && snapshot.scope_hash != expected
        {
            bail!("Scratchpad scope changed concurrently; retry hard trim");
        }
        expected_scope_hash = Some(snapshot.scope_hash.clone());
        let blocks = snapshot.blocks;
        let total: i64 = blocks.iter().map(|b| i64::from(b.bytes)).sum();
        if total <= target_bytes {
            result.over_cap = total > i64::from(cap);
            result.final_scope_hash = expected_scope_hash;
            return Ok(result);
        }
        let Some(target) = next_hard_trim_block(&blocks, &skipped_blocks) else {
            break;
        };
        let trim = hard_trim(
            pool,
            scope_type,
            scope_key,
            target,
            expected_scope_hash.as_deref().expect("snapshot hash"),
            cap,
        )
        .await?;
        if trim.changed {
            expected_scope_hash = trim.scope_hash.clone();
        }
        result.changed |= trim.changed;
        result.data_loss |= trim.data_loss;
        result.mutation_count += u64::from(trim.changed);
        if trim.changed {
            skipped_blocks.clear();
        } else {
            skipped_blocks.push(target.block.clone());
        }
    }

    let final_snapshot = ff_db::queries::pg_memory_scope_snapshot(pool, scope_type, scope_key)
        .await
        .context("verify locked Scratchpad scope after consolidation")?;
    if final_snapshot.cap_bytes != cap
        || expected_scope_hash.as_deref() != Some(final_snapshot.scope_hash.as_str())
    {
        bail!("Scratchpad scope or cap changed concurrently; retry consolidation");
    }
    let total: i64 = final_snapshot
        .blocks
        .iter()
        .map(|block| i64::from(block.bytes))
        .sum();
    result.final_scope_hash = Some(final_snapshot.scope_hash);
    result.over_cap = total > i64::from(cap);
    if result.over_cap {
        warn!(
            scope_type,
            scope_key,
            bytes_used = total,
            cap_bytes = cap,
            "scratchpad: fail-closed consolidation could not reach cap"
        );
    }
    Ok(result)
}

/// Archive a dead `session`-scope scratchpad: push every non-empty block's
/// FULL content into Brain as a knowledge candidate, record an eviction audit
/// row per block, then delete the scope's rows. Called by the dreamer
/// ([`crate::dreamer`]) for session scopes idle past their TTL — the write-path
/// consolidation above can never reach them because writes have stopped.
/// No summarizer involved: the session is over, so the whole text graduates to
/// Brain verbatim and the scratchpad rows are dropped. Idempotent (a re-run on
/// the same scope finds no rows). Returns the number of blocks archived.
pub(crate) async fn archive_session_scope(pool: &PgPool, scope_key: &str) -> Result<usize> {
    let blocks = ff_db::queries::pg_memory_get_all(pool, "session", scope_key).await?;
    let mut archived = 0usize;
    for b in blocks.iter().filter(|b| !b.content.is_empty()) {
        let brain_ref = push_to_brain(pool, "session", scope_key, &b.block, &b.content).await;
        ff_db::queries::pg_memory_record_eviction(
            pool,
            "session",
            scope_key,
            &b.block,
            &hex_sha256(&b.content),
            b.bytes,
            "(archived whole: session-scope TTL sweep)",
            "dreamer",
            brain_ref.as_deref(),
        )
        .await
        .context("record session-archive eviction")?;
        archived += 1;
    }
    sqlx::query("DELETE FROM agent_memory WHERE scope_type = 'session' AND scope_key = $1")
        .bind(scope_key)
        .execute(pool)
        .await
        .context("delete archived session scope")?;
    info!(
        scope_key,
        archived, "scratchpad: archived dead session scope to Brain"
    );
    Ok(archived)
}

/// Hard-trim a block to its newest half (keeps the most recent lines). Never
/// called on `decisions` by the priority order above.
async fn hard_trim(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    block: &MemoryBlock,
    expected_scope_hash: &str,
    expected_cap_bytes: i32,
) -> Result<TrimResult> {
    if !hard_trim_eligible(&block.block) {
        bail!(
            "Scratchpad refuses to hard-trim protected block '{}'",
            block.block
        );
    }
    let trimmed = newest_half(&block.content);
    if trimmed == block.content {
        return Ok(TrimResult {
            changed: false,
            data_loss: false,
            bytes_used: None,
            scope_hash: None,
        });
    }
    let brain_ref = push_to_brain(pool, scope_type, scope_key, &block.block, &block.content).await;
    warn!(
        scope_type, scope_key, block = %block.block,
        "scratchpad: summarizer unavailable — hard-trimmed block to newest half"
    );
    let replacement = ff_db::queries::pg_memory_try_set_block(
        pool,
        MemoryTrySetBlockRequest {
            scope_type,
            scope_key,
            block: &block.block,
            expected_content: &block.content,
            new_content: &trimmed,
            allow_over_cap_repair: true,
            eviction_archive: Some(MemoryEvictionArchive {
                expected_scope_hash,
                expected_cap_bytes,
                prev_bytes: block.bytes,
                result_summary: "hard-trimmed to newest half",
                summarizer: "hard-trim",
                brain_ref: brain_ref.as_deref(),
            }),
        },
    )
    .await
    .context("hard-trim block")?;
    match replacement.status {
        MemoryBlockWriteStatus::Applied => {
            if replacement.eviction_id.is_none() {
                bail!("Scratchpad hard trim committed without eviction evidence");
            }
            Ok(TrimResult {
                changed: true,
                data_loss: true,
                bytes_used: Some(replacement.bytes_used),
                scope_hash: Some(
                    replacement
                        .scope_hash
                        .context("hard trim replacement omitted its scope revision")?,
                ),
            })
        }
        MemoryBlockWriteStatus::Stale => {
            bail!("Scratchpad block changed concurrently; retry hard trim")
        }
        MemoryBlockWriteStatus::Busy => {
            bail!("Scratchpad scope is being updated concurrently; retry hard trim")
        }
        MemoryBlockWriteStatus::OverCap => {
            bail!("Scratchpad rejected a non-reducing hard trim")
        }
    }
}

fn newest_half(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() > 1 {
        return lines[lines.len() / 2..].join("\n");
    }
    let chars: Vec<char> = content.chars().collect();
    chars[chars.len() / 2..].iter().collect()
}

fn summary_makes_progress(content: &str, summary: &str) -> bool {
    summary.len() < content.len()
}

fn next_eligible_block<'a>(
    blocks: &'a [MemoryBlock],
    skipped_blocks: &[String],
) -> Option<&'a MemoryBlock> {
    EVICTION_ORDER.iter().find_map(|name| {
        if skipped_blocks.iter().any(|skipped| skipped == name) {
            return None;
        }
        blocks
            .iter()
            .find(|b| &b.block == name && !b.content.is_empty())
    })
}

fn next_hard_trim_block<'a>(
    blocks: &'a [MemoryBlock],
    skipped_blocks: &[String],
) -> Option<&'a MemoryBlock> {
    HARD_TRIM_ORDER.iter().find_map(|name| {
        if skipped_blocks.iter().any(|skipped| skipped == name) {
            return None;
        }
        blocks
            .iter()
            .find(|b| &b.block == name && !b.content.is_empty())
    })
}

fn compact_eligible_blocks(blocks: &[MemoryBlock]) -> Vec<CompactEligibleBlock> {
    EVICTION_ORDER
        .iter()
        .filter_map(|name| {
            blocks
                .iter()
                .find(|block| block.block == *name && !block.content.is_empty())
                .map(|block| CompactEligibleBlock {
                    block: block.block.clone(),
                    bytes: block.bytes,
                    hard_trim_eligible: hard_trim_eligible(name),
                })
        })
        .collect()
}

fn hard_trim_eligible(block: &str) -> bool {
    HARD_TRIM_ORDER.contains(&block)
}

fn compact_status(
    applied: bool,
    was_over_cap: bool,
    changed: bool,
    data_loss: bool,
    still_over_cap: bool,
) -> String {
    if !applied {
        return if was_over_cap {
            "dry_run_over_cap"
        } else {
            "dry_run_within_cap"
        }
        .to_string();
    }
    match (still_over_cap, changed, data_loss) {
        (true, _, true) => "still_over_cap_data_loss",
        (true, _, false) => "still_over_cap",
        (false, true, true) => "compacted_data_loss",
        (false, true, false) => "compacted",
        (false, false, _) => "already_within_cap",
    }
    .to_string()
}

fn write_status(over_cap: bool, consolidated: bool, data_loss: bool) -> String {
    match (over_cap, consolidated, data_loss) {
        (true, _, true) => "over_cap_data_loss",
        (true, true, false) => "over_cap_after_consolidation",
        (true, false, false) => "over_cap",
        (false, _, true) => "ok_data_loss",
        (false, true, false) => "ok_consolidated",
        (false, false, false) => "ok",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Row, postgres::PgPoolOptions};
    use std::sync::Arc;
    use tokio::sync::Barrier;

    fn temp_db_urls() -> Option<(String, String, String)> {
        let base_url = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
            .ok()?;
        let (prefix, _) = base_url.rsplit_once('/')?;
        let db_name = format!("ff_scratchpad_write_{}", uuid::Uuid::new_v4().simple());
        Some((
            format!("{prefix}/postgres"),
            format!("{prefix}/{db_name}"),
            db_name,
        ))
    }

    async fn scratchpad_test_pool() -> Option<(PgPool, PgPool, String)> {
        let (admin_url, db_url, db_name) = temp_db_urls()?;
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .expect("connect scratchpad test admin db");
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("create scratchpad test db");
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&db_url)
            .await
            .expect("connect scratchpad test db");
        sqlx::query("CREATE EXTENSION IF NOT EXISTS pgcrypto")
            .execute(&pool)
            .await
            .expect("install pgcrypto in scratchpad test db");
        sqlx::raw_sql(ff_db::schema::SCHEMA_V139_AGENT_SCRATCHPAD)
            .execute(&pool)
            .await
            .expect("install canonical Scratchpad tables");
        Some((admin, pool, db_name))
    }

    async fn drop_temp_db(admin: PgPool, pool: PgPool, db_name: &str) {
        pool.close().await;
        sqlx::query(
            "SELECT pg_terminate_backend(pid)
               FROM pg_stat_activity
              WHERE datname = $1 AND pid <> pg_backend_pid()",
        )
        .bind(db_name)
        .execute(&admin)
        .await
        .expect("terminate scratchpad test db sessions");
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("drop scratchpad test db");
    }

    async fn seed_overflow_fixture(pool: &PgPool, scope_key: &str) -> (&'static str, &'static str) {
        const ORIGINAL_SCRATCH: &str = "abcdefghijklmnopqrstuvwxyz0123456789";
        const APPEND: &str = "12345678901234";
        memory_set_cap(pool, "project", scope_key, 48)
            .await
            .expect("set test cap");
        ff_db::queries::pg_memory_set_block(
            pool,
            "project",
            scope_key,
            "scratch",
            ORIGINAL_SCRATCH,
        )
        .await
        .expect("seed scratch block");
        ff_db::queries::pg_memory_set_block(pool, "project", scope_key, "task", "seed")
            .await
            .expect("seed target block");
        (ORIGINAL_SCRATCH, APPEND)
    }

    fn block(name: &str, content: &str) -> MemoryBlock {
        MemoryBlock {
            scope_type: "project".to_string(),
            scope_key: "test".to_string(),
            block: name.to_string(),
            content: content.to_string(),
            bytes: content.len() as i32,
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn scratchpad_selection_advances_past_non_progressing_block() {
        let blocks = vec![
            block("task", "task"),
            block("findings", "findings"),
            block("scratch", "scratch"),
        ];

        let first = next_eligible_block(&blocks, &[]).expect("first eligible");
        assert_eq!(first.block, "scratch");

        let skipped = vec!["scratch".to_string()];
        let next = next_eligible_block(&blocks, &skipped).expect("next eligible");
        assert_eq!(next.block, "findings");
    }

    #[test]
    fn scratchpad_newest_half_shrinks_single_line_content() {
        assert_eq!(newest_half("abcdef"), "def");
        assert_eq!(newest_half("a\nb\nc\nd"), "c\nd");
        assert_eq!(newest_half("é🙂漢字"), "漢字");
    }

    #[test]
    fn scratchpad_rejects_non_shrinking_model_output() {
        assert!(!summary_makes_progress("short", "longer"));
        assert!(!summary_makes_progress("same", "size"));
        assert!(summary_makes_progress("longer", "short"));
    }

    #[test]
    fn scratchpad_hard_trim_advances_through_every_mutable_block() {
        let blocks = vec![
            block("decisions", "preserve"),
            block("task", "task"),
            block("state", "state"),
        ];
        assert_eq!(
            next_hard_trim_block(&blocks, &[])
                .expect("state is hard-trimmable")
                .block,
            "state"
        );
        let skipped = vec!["state".to_string(), "task".to_string()];
        assert!(next_hard_trim_block(&blocks, &skipped).is_none());
        assert!(!hard_trim_eligible("decisions"));
        assert!(hard_trim_eligible("scratch"));
    }

    #[test]
    fn compact_preview_lists_exact_nonempty_blocks_in_eviction_order() {
        let blocks = vec![
            block("decisions", "keep"),
            block("task", "do"),
            block("scratch", "transient"),
            block("state", ""),
        ];
        let eligible = compact_eligible_blocks(&blocks);
        assert_eq!(
            eligible,
            vec![
                CompactEligibleBlock {
                    block: "scratch".to_string(),
                    bytes: 9,
                    hard_trim_eligible: true,
                },
                CompactEligibleBlock {
                    block: "task".to_string(),
                    bytes: 2,
                    hard_trim_eligible: true,
                },
                CompactEligibleBlock {
                    block: "decisions".to_string(),
                    bytes: 4,
                    hard_trim_eligible: false,
                },
            ]
        );
    }

    #[test]
    fn compact_status_distinguishes_preview_success_loss_and_remaining_overage() {
        assert_eq!(
            compact_status(false, true, false, false, true),
            "dry_run_over_cap"
        );
        assert_eq!(
            compact_status(false, false, false, false, false),
            "dry_run_within_cap"
        );
        assert_eq!(compact_status(true, true, true, false, false), "compacted");
        assert_eq!(
            compact_status(true, true, true, true, false),
            "compacted_data_loss"
        );
        assert_eq!(
            compact_status(true, true, false, false, true),
            "still_over_cap"
        );
    }

    #[test]
    fn scratchpad_write_status_reports_over_cap_and_data_loss() {
        assert_eq!(
            write_status(true, true, true),
            "over_cap_data_loss".to_string()
        );
        assert_eq!(
            write_status(true, true, false),
            "over_cap_after_consolidation".to_string()
        );
        assert_eq!(write_status(false, false, false), "ok".to_string());
    }

    #[tokio::test]
    async fn append_over_cap_compacts_applies_and_archives_exact_previous_bytes() {
        let Some((admin, pool, db_name)) = scratchpad_test_pool().await else {
            return;
        };
        let scope_key = format!("append-{}", uuid::Uuid::new_v4().simple());
        let (original_scratch, append) = seed_overflow_fixture(&pool, &scope_key).await;

        let result = memory_add(&pool, "project", &scope_key, "task", append)
            .await
            .expect("append should compact once and apply");

        assert!(result.consolidated);
        assert!(result.data_loss, "missing test summarizer must hard-trim");
        assert!(!result.over_cap);
        assert_eq!(result.status, "ok_data_loss");
        assert!(result.bytes_used <= i64::from(result.cap_bytes));
        assert_eq!(
            ff_db::queries::pg_memory_get_block(&pool, "project", &scope_key, "task")
                .await
                .unwrap(),
            format!("seed\n{append}")
        );

        let archive = sqlx::query(
            "SELECT summary, prev_hash, prev_bytes
               FROM agent_memory_evictions
              WHERE scope_type = 'project' AND scope_key = $1 AND block = 'scratch'
              ORDER BY created_at, id
              LIMIT 1",
        )
        .bind(&scope_key)
        .fetch_one(&pool)
        .await
        .expect("read exact eviction evidence");
        let summary: String = archive.get("summary");
        assert!(summary.contains(&format!("FULL PRE-MUTATION:\n{original_scratch}")));
        assert_eq!(
            archive.get::<String, _>("prev_hash"),
            hex_sha256(original_scratch)
        );
        assert_eq!(
            archive.get::<i32, _>("prev_bytes"),
            original_scratch.len() as i32
        );

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn replacement_after_compaction_makes_retry_stale_without_overwrite() {
        let Some((admin, pool, db_name)) = scratchpad_test_pool().await else {
            return;
        };
        let scope_key = format!("stale-{}", uuid::Uuid::new_v4().simple());
        let (_, append) = seed_overflow_fixture(&pool, &scope_key).await;
        let reached_retry = Arc::new(Barrier::new(2));
        let release_retry = Arc::new(Barrier::new(2));

        let writer_pool = pool.clone();
        let writer_scope = scope_key.clone();
        let writer_reached = reached_retry.clone();
        let writer_release = release_retry.clone();
        let writer = tokio::spawn(async move {
            write_block_with_retry_hook(
                &writer_pool,
                "project",
                &writer_scope,
                "task",
                MemoryEdit::Add(append),
                move || async move {
                    writer_reached.wait().await;
                    writer_release.wait().await;
                },
            )
            .await
        });

        reached_retry.wait().await;
        let compacted_target =
            ff_db::queries::pg_memory_get_block(&pool, "project", &scope_key, "task")
                .await
                .expect("read compacted target");
        let replacement = ff_db::queries::pg_memory_try_set_block(
            &pool,
            MemoryTrySetBlockRequest {
                scope_type: "project",
                scope_key: &scope_key,
                block: "task",
                expected_content: &compacted_target,
                new_content: "replacement-authority",
                allow_over_cap_repair: false,
                eviction_archive: None,
            },
        )
        .await
        .expect("write concurrent replacement");
        assert_eq!(replacement.status, MemoryBlockWriteStatus::Applied);
        release_retry.wait().await;

        let error = writer
            .await
            .expect("join append writer")
            .expect_err("stale retry must fail closed");
        assert!(
            error
                .to_string()
                .contains("requested write was not applied")
        );
        assert_eq!(
            ff_db::queries::pg_memory_get_block(&pool, "project", &scope_key, "task")
                .await
                .unwrap(),
            "replacement-authority"
        );

        drop_temp_db(admin, pool, &db_name).await;
    }

    #[tokio::test]
    async fn no_eligible_space_leaves_append_unapplied() {
        let Some((admin, pool, db_name)) = scratchpad_test_pool().await else {
            return;
        };
        let scope_key = format!("decisions-{}", uuid::Uuid::new_v4().simple());
        let protected = "decision-bytes-must-stay-full";
        memory_set_cap(&pool, "project", &scope_key, 32)
            .await
            .expect("set protected-block cap");
        ff_db::queries::pg_memory_set_block(&pool, "project", &scope_key, "decisions", protected)
            .await
            .expect("seed protected decisions");

        let error = memory_add(&pool, "project", &scope_key, "decisions", "cannot-fit")
            .await
            .expect_err("decisions must never be hard-trimmed for an append");
        assert!(error.to_string().contains("requested write"));
        assert_eq!(
            ff_db::queries::pg_memory_get_block(&pool, "project", &scope_key, "decisions")
                .await
                .unwrap(),
            protected
        );
        let evictions: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_memory_evictions
              WHERE scope_type = 'project' AND scope_key = $1",
        )
        .bind(&scope_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(evictions, 0);

        drop_temp_db(admin, pool, &db_name).await;
    }
}

/// Summarize a block via a cheap fleet model, preserving the durable facts.
async fn summarize_block(pool: &PgPool, block: &str, content: &str) -> Result<String> {
    let (endpoint, model) = resolve_summarizer(pool).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build summarizer http client")?;
    let prompt = format!(
        "You are compacting an AI agent's working-memory block named '{block}'. \
         Rewrite it to roughly HALF its length. PRESERVE every decision, \
         constraint, file path, command, identifier (PR/issue/UUID/port), and \
         recorded failure — drop only transient narration. Output ONLY the \
         compacted text, no preamble.\n\n---\n{content}"
    );
    let target_tokens = (content.len() / 4).clamp(128, 2048) as u32;
    crate::research::openai_single_completion(&endpoint, &model, &prompt, target_tokens, &client)
        .await
        .context("summarizer completion")
}

/// Pick a healthy, least-loaded fleet endpoint+model for the summarizer.
/// Summarization needs no tool-calling, so any healthy chat deployment works.
async fn resolve_summarizer(pool: &PgPool) -> Result<(String, String)> {
    let filter = ff_db::RouteFilter {
        workload: None,
        require_tool_calling: false,
        min_ctx: None,
        exclude_hosts: vec![],
        max_health_age_sec: Some(ff_db::queries::DISPATCH_HEALTH_MAX_AGE_SEC),
        prefer_least_loaded: true,
        limit: 8,
    };
    let candidates = ff_db::pg_route_deployments(pool, &filter)
        .await
        .context("route a summarizer endpoint")?;
    let c = candidates
        .into_iter()
        .next()
        .context("no healthy LLM deployment available for summarization")?;
    let model = c.catalog_id.or(c.catalog_name).unwrap_or_default();
    Ok((c.endpoint, model))
}

/// Push evicted full content into Brain as a candidate. Best-effort: returns
/// the candidate id on success, `None` (logged) on any failure — the eviction
/// audit row is the durable record regardless.
async fn push_to_brain(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    block: &str,
    content: &str,
) -> Option<String> {
    let user = match ff_db::pg_get_brain_user(pool, DEFAULT_USER).await {
        Ok(Some(u)) => u.id,
        Ok(None) => match ff_db::pg_create_brain_user(pool, DEFAULT_USER, Some("Venkat")).await {
            Ok(id) => id,
            Err(e) => {
                warn!(error = %e, "scratchpad: brain push skipped (create user failed)");
                return None;
            }
        },
        Err(e) => {
            warn!(error = %e, "scratchpad: brain push skipped (resolve user failed)");
            return None;
        }
    };
    let title = format!("working-memory eviction: {scope_type}:{scope_key} / {block}");
    let tags = vec!["working-memory".to_string(), block.to_string()];
    match ff_db::pg_insert_brain_candidate(
        pool,
        user,
        None,
        "create",
        Some("working-memory-eviction"),
        Some(&title),
        Some(content),
        &tags,
        None,
        None,
        None,
        Some(0.5),
    )
    .await
    {
        Ok(id) => Some(id.to_string()),
        Err(e) => {
            warn!(error = %e, "scratchpad: brain push failed (non-fatal)");
            None
        }
    }
}

fn hex_sha256(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Render the frozen working-memory snapshot for injection at session start.
/// Tool writes hit Postgres and surface in the *next* snapshot — the live
/// prompt is never mutated mid-session (preserves prompt caching).
pub async fn render_snapshot(pool: &PgPool, scope_type: &str, scope_key: &str) -> Result<String> {
    let blocks = memory_get(pool, scope_type, scope_key, None).await?;
    if blocks.is_empty() {
        return Ok(String::new());
    }
    let mut out =
        String::from("## Scratchpad (curated working memory — edit via memory_* tools)\n");
    for b in blocks {
        out.push_str(&format!("### {}\n{}\n", b.block, b.content));
    }
    Ok(out)
}
