//! Agent working memory — the "Scratchpad".
//!
//! A small, byte-capped, agent-self-editable text surface with fixed blocks
//! and layered scope. When a write pushes a scope over its byte cap, the
//! lowest-priority block is summarized (consolidate-and-forget) and its full
//! pre-summary content is pushed down into Brain as a candidate so nothing is
//! truly lost. Sits *beside* `session_brain`, *above* Brain/Cortex/Vault.
//!
//! ff-db owns the transactional SQL primitives (`pg_memory_*`); this module
//! owns the string-edit ops (`add`/`replace`/`remove`) and the
//! consolidate-and-forget driver (which calls a summarizer LLM).
//!
//! Design: `plans/agent-working-memory.md` (LLM council 2026-06-19).

use anyhow::{Context, Result, bail};
use ff_db::queries::{MEMORY_BLOCKS, MemoryBlock};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{info, warn};

const DEFAULT_USER: &str = "venkat";

/// Eviction priority: `scratch` first, `decisions` last (only ever summarized).
const EVICTION_ORDER: [&str; 5] = ["scratch", "findings", "state", "task", "decisions"];

/// Max consolidate-and-forget passes before falling back to a hard trim.
const MAX_CONSOLIDATE_PASSES: usize = 5;

/// Result of a memory write, mirrored back to the caller / tool response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WriteResult {
    pub scope_type: String,
    pub scope_key: String,
    pub block: String,
    pub bytes_used: i64,
    pub cap_bytes: i32,
    pub consolidated: bool,
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
    write_block(pool, scope_type, scope_key, block, &next).await
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
    write_block(pool, scope_type, scope_key, block, &next).await
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
    let next = match text {
        None => String::new(),
        Some(t) => {
            let cur =
                ff_db::queries::pg_memory_get_block(pool, scope_type, scope_key, block).await?;
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
    write_block(pool, scope_type, scope_key, block, &next).await
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

/// Write a block's full new content, then enforce the scope's byte cap by
/// consolidating if needed. Shared tail of every edit op.
async fn write_block(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    block: &str,
    content: &str,
) -> Result<WriteResult> {
    ff_db::queries::pg_memory_set_block(pool, scope_type, scope_key, block, content)
        .await
        .context("write memory block")?;

    let cap = ff_db::queries::pg_memory_cap(pool, scope_type, scope_key).await?;
    let mut total = ff_db::queries::pg_memory_total_bytes(pool, scope_type, scope_key).await?;
    let mut consolidated = false;

    if total > cap as i64 {
        consolidated = consolidate_and_forget(pool, scope_type, scope_key, cap).await?;
        total = ff_db::queries::pg_memory_total_bytes(pool, scope_type, scope_key).await?;
    }

    Ok(WriteResult {
        scope_type: scope_type.to_string(),
        scope_key: scope_key.to_string(),
        block: block.to_string(),
        bytes_used: total,
        cap_bytes: cap,
        consolidated,
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
) -> Result<bool> {
    let mut did_anything = false;

    for _ in 0..MAX_CONSOLIDATE_PASSES {
        let blocks = ff_db::queries::pg_memory_get_all(pool, scope_type, scope_key).await?;
        let total: i64 = blocks.iter().map(|b| b.bytes as i64).sum();
        if total <= cap as i64 {
            return Ok(did_anything);
        }

        // Pick the highest-priority-to-evict block that actually has content.
        let target = EVICTION_ORDER.iter().find_map(|name| {
            blocks
                .iter()
                .find(|b| &b.block == name && !b.content.is_empty())
        });
        let Some(target) = target else {
            break; // nothing left to evict
        };

        let summary = match summarize_block(pool, &target.block, &target.content).await {
            Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
            Ok(_) | Err(_) => {
                // Summarizer unavailable/empty → hard-trim backstop this pass.
                hard_trim(pool, scope_type, scope_key, target).await?;
                did_anything = true;
                continue;
            }
        };

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

        ff_db::queries::pg_memory_set_block(pool, scope_type, scope_key, &target.block, &summary)
            .await
            .context("replace block with summary")?;
        did_anything = true;
        info!(
            scope_type, scope_key, block = %target.block,
            prev_bytes = target.bytes, brain = brain_ref.is_some(),
            "scratchpad: consolidated block"
        );
    }

    // Final backstop: if still over cap, hard-trim scratch then findings.
    let blocks = ff_db::queries::pg_memory_get_all(pool, scope_type, scope_key).await?;
    let total: i64 = blocks.iter().map(|b| b.bytes as i64).sum();
    if total > cap as i64 {
        for name in ["scratch", "findings"] {
            if let Some(b) = blocks
                .iter()
                .find(|b| b.block == name && !b.content.is_empty())
            {
                hard_trim(pool, scope_type, scope_key, b).await?;
                did_anything = true;
            }
        }
    }
    Ok(did_anything)
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
) -> Result<()> {
    let lines: Vec<&str> = block.content.lines().collect();
    let keep_from = lines.len() / 2;
    let trimmed: String = lines[keep_from..].join("\n");
    warn!(
        scope_type, scope_key, block = %block.block,
        "scratchpad: summarizer unavailable — hard-trimmed block to newest half"
    );
    ff_db::queries::pg_memory_set_block(pool, scope_type, scope_key, &block.block, &trimmed)
        .await
        .context("hard-trim block")
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
