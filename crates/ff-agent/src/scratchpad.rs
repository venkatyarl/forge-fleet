//! Agent working memory — the "Scratchpad".
//!
//! A small, byte-capped, agent-self-editable text surface with fixed blocks
//! and layered scope. When a write pushes a scope over its byte cap, the
//! complete scope is archived verbatim and replaced by a verified pointer.
//! Sits *beside* `session_brain`, *above* Brain/Cortex/Vault.
//!
//! ff-db owns the transactional SQL primitives (`pg_memory_*`); this module
//! owns the string-edit ops (`add`/`replace`/`remove`) and fail-closed repair.
//!
//! Design: `plans/agent-working-memory.md` (LLM council 2026-06-19).

use anyhow::{Context, Result, bail};
use ff_db::queries::{MEMORY_BLOCKS, MemoryBlock};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tracing::{info, warn};

const DEFAULT_USER: &str = "venkat";

const ARCHIVE_VERSION: &str = "memory-integrity-v1";
const ARCHIVE_REASON: &str = "oversized-scope-repair";
const POINTER_PREFIX: &str = "[forgefleet-memory-archive:";

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

/// Write a block's full new content, then enforce the scope's byte cap.
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

/// Compatibility entry point for the dreamer's cap re-enforcement sweep.
pub(crate) async fn consolidate_and_forget(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    cap: i32,
) -> Result<bool> {
    repair_oversized_scope(pool, scope_type, scope_key, cap).await
}

/// Fail-closed repair for an oversized scope. The complete, deterministically
/// serialized scope is archived and read-back verified in the same transaction
/// before any working-memory row is changed.
pub(crate) async fn repair_oversized_scope(
    pool: &PgPool,
    scope_type: &str,
    scope_key: &str,
    cap: i32,
) -> Result<bool> {
    let mut tx = pool.begin().await.context("begin oversized-scope repair")?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("agent-memory:{scope_type}:{scope_key}"))
        .execute(&mut *tx)
        .await
        .context("lock memory scope")?;
    let rows = sqlx::query(
        "SELECT block, content FROM agent_memory
          WHERE scope_type=$1 AND scope_key=$2 ORDER BY block FOR UPDATE",
    )
    .bind(scope_type)
    .bind(scope_key)
    .fetch_all(&mut *tx)
    .await
    .context("read locked memory scope")?;
    let total: usize = rows
        .iter()
        .map(|r| r.get::<String, _>("content").len())
        .sum();
    if total <= cap.max(0) as usize {
        tx.commit().await?;
        return Ok(false);
    }
    if rows.len() == 1
        && rows[0]
            .get::<String, _>("content")
            .starts_with(POINTER_PREFIX)
    {
        bail!("archived pointer exceeds configured cap; refusing to re-archive it");
    }

    let blocks: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "block": r.get::<String, _>("block"),
                "content": r.get::<String, _>("content")
            })
        })
        .collect();
    let archive = serde_json::to_string(&serde_json::json!({
        "version": ARCHIVE_VERSION,
        "reason": ARCHIVE_REASON,
        "scope_type": scope_type,
        "scope_key": scope_key,
        "blocks": blocks
    }))?;
    let hash = hex_sha256(&archive);
    let user_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO brain_users(name, display_name) VALUES ($1, 'Venkat')
         ON CONFLICT(name) DO UPDATE SET display_name=COALESCE(brain_users.display_name, EXCLUDED.display_name)
         RETURNING id",
    )
    .bind(DEFAULT_USER)
    .fetch_one(&mut *tx)
    .await
    .context("resolve archive owner")?;
    let archive_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO brain_knowledge_candidates
           (user_id, action, kind, title, body, tags, project, confidence)
         VALUES ($1, 'create', $2, $3, $4, $5, $6, 1.0) RETURNING id",
    )
    .bind(user_id)
    .bind("working-memory-scope-archive")
    .bind(format!("memory archive: {scope_type}:{scope_key}"))
    .bind(&archive)
    .bind(vec![
        "working-memory".to_string(),
        ARCHIVE_VERSION.to_string(),
    ])
    .bind(scope_key)
    .fetch_one(&mut *tx)
    .await
    .context("archive complete memory scope")?;
    let restored: String =
        sqlx::query_scalar("SELECT body FROM brain_knowledge_candidates WHERE id=$1 FOR SHARE")
            .bind(archive_id)
            .fetch_one(&mut *tx)
            .await
            .context("read back memory archive")?;
    if restored.as_bytes().len() != archive.as_bytes().len() || hex_sha256(&restored) != hash {
        bail!("memory archive verification failed");
    }
    let locator = archive_id.to_string();
    let pointer = format!(
        "{POINTER_PREFIX}{ARCHIVE_VERSION}] locator=brain-candidate:{locator} sha256={hash} bytes={} reason={ARCHIVE_REASON}",
        archive.len()
    );
    if pointer.len() > cap.max(0) as usize {
        bail!("memory cap {cap} is too small for recoverable archive pointer");
    }
    sqlx::query(
        "INSERT INTO agent_memory_evictions
          (scope_type, scope_key, block, prev_hash, prev_bytes, summary, summarizer, brain_ref)
         VALUES ($1,$2,'__scope__',$3,$4,$5,$6,$7)",
    )
    .bind(scope_type)
    .bind(scope_key)
    .bind(&hash)
    .bind(i32::try_from(archive.len()).context("archive exceeds audit byte range")?)
    .bind(&pointer)
    .bind(format!("{ARCHIVE_REASON}:{ARCHIVE_VERSION}"))
    .bind(&locator)
    .execute(&mut *tx)
    .await
    .context("record immutable memory eviction evidence")?;
    sqlx::query("DELETE FROM agent_memory WHERE scope_type=$1 AND scope_key=$2")
        .bind(scope_type)
        .bind(scope_key)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO agent_memory(scope_type,scope_key,block,content,bytes,updated_at)
         VALUES($1,$2,'state',$3,octet_length($3),NOW())",
    )
    .bind(scope_type)
    .bind(scope_key)
    .bind(&pointer)
    .execute(&mut *tx)
    .await?;
    let post: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(bytes),0)::bigint FROM agent_memory WHERE scope_type=$1 AND scope_key=$2",
    )
    .bind(scope_type)
    .bind(scope_key)
    .fetch_one(&mut *tx)
    .await?;
    if post > cap as i64 {
        bail!("oversized-scope repair postcondition failed: {post} > {cap}");
    }
    tx.commit().await.context("commit verified memory repair")?;
    Ok(true)
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> Option<PgPool> {
        let url = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
            .ok()?;
        PgPool::connect(&url).await.ok()
    }

    #[tokio::test]
    async fn oversized_scope_is_recoverable_utf8_and_repeat_is_noop() -> Result<()> {
        let Some(pool) = test_pool().await else {
            return Ok(());
        };
        let key = format!("memory-integrity-test-{}", uuid::Uuid::new_v4());
        let original = "🧠é".repeat(25_000);
        ff_db::queries::pg_memory_set_block(&pool, "project", &key, "state", &original).await?;

        assert!(repair_oversized_scope(&pool, "project", &key, 6144).await?);
        let pointer = ff_db::queries::pg_memory_get_block(&pool, "project", &key, "state").await?;
        assert!(pointer.starts_with(POINTER_PREFIX));
        assert!(pointer.len() <= 6144);
        assert!(!repair_oversized_scope(&pool, "project", &key, 6144).await?);

        let locator = pointer
            .split("locator=brain-candidate:")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .context("pointer locator")?;
        let body: String =
            sqlx::query_scalar("SELECT body FROM brain_knowledge_candidates WHERE id::text=$1")
                .bind(locator)
                .fetch_one(&pool)
                .await?;
        let archived: serde_json::Value = serde_json::from_str(&body)?;
        assert_eq!(archived["blocks"][0]["content"], original);
        assert!(pointer.contains(&format!("sha256={}", hex_sha256(&body))));

        sqlx::query("DELETE FROM agent_memory WHERE scope_type='project' AND scope_key=$1")
            .bind(&key)
            .execute(&pool)
            .await?;
        sqlx::query(
            "DELETE FROM agent_memory_evictions WHERE scope_type='project' AND scope_key=$1",
        )
        .bind(&key)
        .execute(&pool)
        .await?;
        sqlx::query("DELETE FROM brain_knowledge_candidates WHERE id::text=$1")
            .bind(locator)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn too_small_cap_rolls_back_without_archive_or_mutation() -> Result<()> {
        let Some(pool) = test_pool().await else {
            return Ok(());
        };
        let key = format!("memory-integrity-rollback-{}", uuid::Uuid::new_v4());
        let original = "state".repeat(20_000);
        ff_db::queries::pg_memory_set_block(&pool, "project", &key, "state", &original).await?;
        assert!(
            repair_oversized_scope(&pool, "project", &key, 1)
                .await
                .is_err()
        );
        assert_eq!(
            ff_db::queries::pg_memory_get_block(&pool, "project", &key, "state").await?,
            original
        );
        let evidence: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM agent_memory_evictions WHERE scope_type='project' AND scope_key=$1",
        )
        .bind(&key)
        .fetch_one(&pool)
        .await?;
        assert_eq!(evidence, 0);
        sqlx::query("DELETE FROM agent_memory WHERE scope_type='project' AND scope_key=$1")
            .bind(&key)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_repairs_create_one_archive_pointer() -> Result<()> {
        let Some(pool) = test_pool().await else {
            return Ok(());
        };
        let key = format!("memory-integrity-concurrency-{}", uuid::Uuid::new_v4());
        ff_db::queries::pg_memory_set_block(&pool, "project", &key, "state", &"x".repeat(100_000))
            .await?;
        let (a, b) = tokio::join!(
            repair_oversized_scope(&pool, "project", &key, 6144),
            repair_oversized_scope(&pool, "project", &key, 6144)
        );
        assert_ne!(a?, b?);
        let evidence: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM agent_memory_evictions WHERE scope_type='project' AND scope_key=$1",
        )
        .bind(&key)
        .fetch_one(&pool)
        .await?;
        assert_eq!(evidence, 1);
        let locator: Option<String> = sqlx::query_scalar(
            "SELECT brain_ref FROM agent_memory_evictions WHERE scope_type='project' AND scope_key=$1",
        )
        .bind(&key)
        .fetch_one(&pool)
        .await?;
        sqlx::query("DELETE FROM agent_memory WHERE scope_type='project' AND scope_key=$1")
            .bind(&key)
            .execute(&pool)
            .await?;
        sqlx::query(
            "DELETE FROM agent_memory_evictions WHERE scope_type='project' AND scope_key=$1",
        )
        .bind(&key)
        .execute(&pool)
        .await?;
        if let Some(locator) = locator {
            sqlx::query("DELETE FROM brain_knowledge_candidates WHERE id::text=$1")
                .bind(locator)
                .execute(&pool)
                .await?;
        }
        Ok(())
    }
}
