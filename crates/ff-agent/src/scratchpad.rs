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
use ff_db::queries::{MEMORY_BLOCKS, MemoryBlock, MemoryBlockWriteStatus};
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

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ConsolidationResult {
    pub changed: bool,
    pub data_loss: bool,
    pub over_cap: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct TrimResult {
    changed: bool,
    data_loss: bool,
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
    let cur = ff_db::queries::pg_memory_get_block(pool, scope_type, scope_key, block).await?;
    let next = if cur.is_empty() {
        text.to_string()
    } else {
        format!("{cur}\n{text}")
    };
    write_block(pool, scope_type, scope_key, block, &cur, &next).await
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
    if old.is_empty() {
        bail!("memory_replace: 'old' must be non-empty");
    }
    let cur = ff_db::queries::pg_memory_get_block(pool, scope_type, scope_key, block).await?;
    let matches = cur.matches(old).count();
    if matches == 0 {
        bail!("memory_replace: 'old' not found in block '{block}'");
    }
    if matches > 1 {
        bail!("memory_replace: 'old' matches {matches}× in block '{block}' (must be unique)");
    }
    let next = cur.replacen(old, new, 1);
    write_block(pool, scope_type, scope_key, block, &cur, &next).await
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
    let cur = ff_db::queries::pg_memory_get_block(pool, scope_type, scope_key, block).await?;
    let next = match text {
        None => String::new(),
        Some(t) => {
            match cur.find(t) {
                Some(idx) => {
                    let mut s = cur.clone();
                    s.replace_range(idx..idx + t.len(), "");
                    // collapse a doubled newline left behind by the removal
                    s.replace("\n\n", "\n").trim().to_string()
                }
                None => bail!("memory_remove: text not found in block '{block}'"),
            }
        }
    };
    write_block(pool, scope_type, scope_key, block, &cur, &next).await
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

/// Compare-and-set a block's full new content without ever growing the scope
/// above its cap. Background consolidation repairs legacy over-cap scopes;
/// foreground callers fail closed instead of committing an oversized write.
async fn write_block(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    block: &str,
    expected_content: &str,
    content: &str,
) -> Result<WriteResult> {
    let write = ff_db::queries::pg_memory_try_set_block(
        pool,
        scope_type,
        scope_key,
        block,
        expected_content,
        content,
        false,
    )
    .await
    .context("write bounded memory block")?;
    match write.status {
        MemoryBlockWriteStatus::Applied => {}
        MemoryBlockWriteStatus::Busy => {
            bail!("Scratchpad scope is being updated concurrently; retry the write")
        }
        MemoryBlockWriteStatus::Stale => {
            bail!("Scratchpad block changed concurrently; read it again before retrying")
        }
        MemoryBlockWriteStatus::OverCap => bail!(
            "Scratchpad write rejected: resulting scope would use {}/{} bytes; remove or replace existing content first",
            write.bytes_used,
            write.cap_bytes
        ),
    }

    Ok(WriteResult {
        scope_type: scope_type.to_string(),
        scope_key: scope_key.to_string(),
        block: block.to_string(),
        bytes_used: write.bytes_used,
        cap_bytes: write.cap_bytes,
        consolidated: false,
        over_cap: write.over_cap,
        data_loss: false,
        status: write_status(write.over_cap, false, false),
    })
}

/// Consolidate-and-forget: while the scope is over `cap`, pick the
/// lowest-priority non-empty block, summarize it (preserving decisions /
/// paths / commands / IDs / failures), push the full pre-summary content into
/// Brain, record an eviction row, and replace the block with the summary.
/// Falls back to a hard trim if the summarizer is unavailable.
/// `pub(crate)` for the dreamer's cap re-enforcement sweep.
pub(crate) async fn consolidate_and_forget(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    cap: i32,
) -> Result<ConsolidationResult> {
    let mut result = ConsolidationResult::default();
    let mut skipped_blocks: Vec<String> = Vec::new();

    for _ in 0..MAX_CONSOLIDATE_PASSES {
        let blocks = ff_db::queries::pg_memory_get_all(pool, scope_type, scope_key).await?;
        let total: i64 = blocks.iter().map(|b| b.bytes as i64).sum();
        if total <= cap as i64 {
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
                let trim = hard_trim(pool, scope_type, scope_key, target).await?;
                result.changed |= trim.changed;
                result.data_loss |= trim.data_loss;
                let after =
                    ff_db::queries::pg_memory_total_bytes(pool, scope_type, scope_key).await?;
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
            let trim = hard_trim(pool, scope_type, scope_key, target).await?;
            result.changed |= trim.changed;
            result.data_loss |= trim.data_loss;
            let after = ff_db::queries::pg_memory_total_bytes(pool, scope_type, scope_key).await?;
            if after < total {
                skipped_blocks.clear();
            } else {
                skipped_blocks.push(target.block.clone());
            }
            continue;
        }

        // Push full pre-summary content down to Brain (best-effort).
        let prev_hash = hex_sha256(&target.content);
        let brain_ref =
            push_to_brain(pool, scope_type, scope_key, &target.block, &target.content).await;

        ff_db::queries::pg_memory_record_eviction(
            pool,
            scope_type,
            scope_key,
            &target.block,
            &prev_hash,
            target.bytes,
            &summary,
            "fleet-summarizer",
            brain_ref.as_deref(),
        )
        .await
        .context("record memory eviction")?;

        let replacement = ff_db::queries::pg_memory_try_set_block(
            pool,
            scope_type,
            scope_key,
            &target.block,
            &target.content,
            &summary,
            true,
        )
        .await
        .context("replace block with bounded summary")?;
        match replacement.status {
            MemoryBlockWriteStatus::Applied => result.changed = true,
            MemoryBlockWriteStatus::Stale => {
                skipped_blocks.push(target.block.clone());
                continue;
            }
            MemoryBlockWriteStatus::Busy => {
                bail!("Scratchpad scope is being updated concurrently; retry consolidation")
            }
            MemoryBlockWriteStatus::OverCap => {
                bail!("Scratchpad rejected a non-reducing consolidation")
            }
        }
        let after = ff_db::queries::pg_memory_total_bytes(pool, scope_type, scope_key).await?;
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
        let blocks = ff_db::queries::pg_memory_get_all(pool, scope_type, scope_key).await?;
        let total: i64 = blocks.iter().map(|b| i64::from(b.bytes)).sum();
        if total <= i64::from(cap) {
            return Ok(result);
        }
        let Some(target) = next_hard_trim_block(&blocks, &skipped_blocks) else {
            break;
        };
        let trim = hard_trim(pool, scope_type, scope_key, target).await?;
        result.changed |= trim.changed;
        result.data_loss |= trim.data_loss;
        if trim.changed {
            skipped_blocks.clear();
        } else {
            skipped_blocks.push(target.block.clone());
        }
    }

    let total = ff_db::queries::pg_memory_total_bytes(pool, scope_type, scope_key).await?;
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
) -> Result<TrimResult> {
    let trimmed = newest_half(&block.content);
    if trimmed == block.content {
        return Ok(TrimResult {
            changed: false,
            data_loss: false,
        });
    }
    let brain_ref = push_to_brain(pool, scope_type, scope_key, &block.block, &block.content).await;
    let audit_summary = format!(
        "(hard-trimmed to newest half; full pre-trim content preserved below)\n\n{}",
        block.content
    );
    ff_db::queries::pg_memory_record_eviction(
        pool,
        scope_type,
        scope_key,
        &block.block,
        &hex_sha256(&block.content),
        block.bytes,
        &audit_summary,
        "hard-trim",
        brain_ref.as_deref(),
    )
    .await
    .context("record hard-trim eviction")?;
    warn!(
        scope_type, scope_key, block = %block.block,
        "scratchpad: summarizer unavailable — hard-trimmed block to newest half"
    );
    let replacement = ff_db::queries::pg_memory_try_set_block(
        pool,
        scope_type,
        scope_key,
        &block.block,
        &block.content,
        &trimmed,
        true,
    )
    .await
    .context("hard-trim block")?;
    match replacement.status {
        MemoryBlockWriteStatus::Applied => Ok(TrimResult {
            changed: true,
            data_loss: true,
        }),
        MemoryBlockWriteStatus::Stale => Ok(TrimResult::default()),
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
