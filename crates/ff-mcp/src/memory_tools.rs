//! MCP tools for the agent Scratchpad (working memory).
//!
//! Thin wrappers over `ff_agent::scratchpad`. The driver owns cap enforcement
//! and consolidate-and-forget; these handlers only parse params and shape JSON.
//! Design: `.forgefleet/plans/agent-working-memory.md`.

use ff_agent::scratchpad;
use serde_json::{Value, json};

use crate::handlers::HandlerResult;

async fn get_pool() -> Result<sqlx::PgPool, String> {
    crate::pool::shared_pg_pool().await
}

fn str_param<'a>(p: &'a Value, key: &str) -> Result<&'a str, String> {
    p.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing required parameter: {key}"))
}

/// Default scope when the caller omits one: a session-scoped pad keyed `default`.
fn scope_of(p: &Value) -> (String, String) {
    let st = p
        .get("scope_type")
        .and_then(|v| v.as_str())
        .unwrap_or("session")
        .to_string();
    let sk = p
        .get("scope_key")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    (st, sk)
}

fn write_json(r: scratchpad::WriteResult) -> Value {
    json!({
        "scope_type": r.scope_type,
        "scope_key": r.scope_key,
        "block": r.block,
        "bytes_used": r.bytes_used,
        "cap_bytes": r.cap_bytes,
        "consolidated": r.consolidated,
    })
}

/// Read the working set — all blocks, or a single `block`.
pub async fn memory_get(params: Option<Value>) -> HandlerResult {
    let p = params.unwrap_or(json!({}));
    let (st, sk) = scope_of(&p);
    let block = p.get("block").and_then(|v| v.as_str());
    let pool = get_pool().await?;
    let blocks = scratchpad::memory_get(&pool, &st, &sk, block)
        .await
        .map_err(|e| e.to_string())?;
    let out: Vec<Value> = blocks
        .iter()
        .map(|b| {
            json!({
                "block": b.block,
                "content": b.content,
                "bytes": b.bytes,
                "updated_at": b.updated_at.to_rfc3339(),
            })
        })
        .collect();
    Ok(json!({ "scope_type": st, "scope_key": sk, "blocks": out }))
}

/// Append text to a block.
pub async fn memory_add(params: Option<Value>) -> HandlerResult {
    let p = params.ok_or_else(|| "missing parameters".to_string())?;
    let (st, sk) = scope_of(&p);
    let block = str_param(&p, "block")?;
    let text = str_param(&p, "text")?;
    let pool = get_pool().await?;
    scratchpad::memory_add(&pool, &st, &sk, block, text)
        .await
        .map(write_json)
        .map_err(|e| e.to_string())
}

/// Replace the single occurrence of `old` with `new` in a block.
pub async fn memory_replace(params: Option<Value>) -> HandlerResult {
    let p = params.ok_or_else(|| "missing parameters".to_string())?;
    let (st, sk) = scope_of(&p);
    let block = str_param(&p, "block")?;
    let old = str_param(&p, "old")?;
    let new = str_param(&p, "new")?;
    let pool = get_pool().await?;
    scratchpad::memory_replace(&pool, &st, &sk, block, old, new)
        .await
        .map(write_json)
        .map_err(|e| e.to_string())
}

/// Remove one occurrence of `text` from a block, or clear it when `text` omitted.
pub async fn memory_remove(params: Option<Value>) -> HandlerResult {
    let p = params.ok_or_else(|| "missing parameters".to_string())?;
    let (st, sk) = scope_of(&p);
    let block = str_param(&p, "block")?;
    let text = p.get("text").and_then(|v| v.as_str());
    let pool = get_pool().await?;
    scratchpad::memory_remove(&pool, &st, &sk, block, text)
        .await
        .map(write_json)
        .map_err(|e| e.to_string())
}
