//! `ff memory` — agent working memory (Scratchpad) CLI.
//!
//! Thin CLI over `ff_agent::scratchpad`. Mirrors the MCP `memory_*` tools so
//! the same bounded, self-curating memory is reachable from the shell.

use crate::{CYAN, RESET};
use anyhow::Result;
use ff_agent::scratchpad;

pub async fn handle_memory(cmd: crate::MemoryCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::MemoryCommand::Get {
            scope_type,
            scope_key,
            block,
        } => {
            let (scope_type, scope_key) = auto_scope(scope_type, scope_key);
            let blocks = scratchpad::memory_get(&pool, &scope_type, &scope_key, block.as_deref())
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            if blocks.is_empty() {
                println!("(empty — no working memory for {scope_type}:{scope_key})");
                return Ok(());
            }
            let cap = ff_db::queries::pg_memory_cap(&pool, &scope_type, &scope_key).await?;
            let total: i64 = blocks.iter().map(|b| b.bytes as i64).sum();
            println!("{CYAN}▶ Scratchpad {scope_type}:{scope_key} — {total}/{cap} bytes{RESET}");
            for b in blocks {
                println!("\n{CYAN}### {} ({} B){RESET}", b.block, b.bytes);
                println!("{}", b.content);
            }
        }
        crate::MemoryCommand::Add {
            block,
            text,
            scope_type,
            scope_key,
        } => {
            let (scope_type, scope_key) = auto_scope(scope_type, scope_key);
            let r = scratchpad::memory_add(&pool, &scope_type, &scope_key, &block, &text)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            print_write(&r);
        }
        crate::MemoryCommand::Replace {
            block,
            old,
            new,
            scope_type,
            scope_key,
        } => {
            let (scope_type, scope_key) = auto_scope(scope_type, scope_key);
            let r = scratchpad::memory_replace(&pool, &scope_type, &scope_key, &block, &old, &new)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            print_write(&r);
        }
        crate::MemoryCommand::Remove {
            block,
            text,
            scope_type,
            scope_key,
        } => {
            let (scope_type, scope_key) = auto_scope(scope_type, scope_key);
            let r =
                scratchpad::memory_remove(&pool, &scope_type, &scope_key, &block, text.as_deref())
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
            print_write(&r);
        }
        crate::MemoryCommand::Cap {
            cap_bytes,
            scope_type,
            scope_key,
        } => {
            scratchpad::memory_set_cap(&pool, &scope_type, &scope_key, cap_bytes)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            let target = if scope_key.is_empty() {
                format!("{scope_type} (default)")
            } else {
                format!("{scope_type}:{scope_key}")
            };
            println!("{CYAN}✓ cap for {target} set to {cap_bytes} bytes{RESET}");
        }
    }
    Ok(())
}

fn print_write(r: &scratchpad::WriteResult) {
    let flag = if r.consolidated {
        " (consolidated — over cap, summarized)"
    } else {
        ""
    };
    println!(
        "{CYAN}✓ {}:{} / {} — {}/{} bytes{}{RESET}",
        r.scope_type, r.scope_key, r.block, r.bytes_used, r.cap_bytes, flag
    );
}

// Project-scoping (council verdict 2026-06-19, decision → Brain). When the caller
// leaves scope at the defaults (session/default), derive a stable project id from
// the process cwd so memory is SHARED per-project across CLIs (Claude Code's
// project memory recalled by Codex on the same repo). An explicit --scope-type /
// --scope-key always wins. The resolver is shared with the memory_* MCP tools via
// `ff_agent::project_scope` (single canonicalization, no drift).
fn auto_scope(scope_type: String, scope_key: String) -> (String, String) {
    if scope_type == "session"
        && scope_key == "default"
        && let Some(id) = ff_agent::project_scope::resolve_from_dir(None)
    {
        return ("project".to_string(), id);
    }
    (scope_type, scope_key)
}
