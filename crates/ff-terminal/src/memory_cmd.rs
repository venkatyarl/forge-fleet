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
    // Compact's default preview is strictly read-only and must not apply
    // unrelated schema migrations as a side effect. Apply is deliberately
    // narrow too; it operates only on the already-existing Scratchpad tables.
    if !matches!(&cmd, crate::MemoryCommand::Compact { .. }) {
        ff_db::run_postgres_migrations(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
    }

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
        crate::MemoryCommand::Compact {
            scope_type,
            scope_key,
            apply,
        } => {
            let (scope_type, scope_key) = auto_scope(scope_type, scope_key);
            let result = scratchpad::memory_compact(&pool, &scope_type, &scope_key, apply)
                .await
                .map_err(|error| anyhow::anyhow!("compact {scope_type}:{scope_key}: {error}"))?;
            print!("{}", render_compact_result(&result));
        }
    }
    Ok(())
}

fn render_compact_result(result: &scratchpad::MemoryCompactResult) -> String {
    let mode = if result.applied { "apply" } else { "dry-run" };
    let mut out = format!(
        "Scratchpad compact {mode}\n  scope: {}:{}\n  bytes: {} -> {} / {} cap\n",
        result.scope_type,
        result.scope_key,
        result.before_bytes,
        result.after_bytes,
        result.cap_bytes,
    );
    out.push_str(&format!(
        "  eligible blocks ({}):\n",
        result.eligible_blocks.len()
    ));
    if result.eligible_blocks.is_empty() {
        out.push_str("    (none)\n");
    } else {
        for block in &result.eligible_blocks {
            let policy = if block.hard_trim_eligible {
                "summary + archive-before-hard-trim"
            } else {
                "summary only (never hard-trimmed)"
            };
            out.push_str(&format!(
                "    {}: {} bytes — {policy}\n",
                block.block, block.bytes
            ));
        }
    }
    out.push_str(&format!(
        "  result: status={} changed={} data_loss={} still_over_cap={}\n",
        result.status, result.changed, result.data_loss, result.still_over_cap
    ));
    out.push_str(&format!(
        "  durable evidence: evictions={} (+{}), Brain candidates={} (+{})\n",
        result.eviction_evidence,
        result.evictions_created,
        result.brain_evidence,
        result.brain_candidates_created,
    ));
    if !result.applied {
        out.push_str("  no changes made; rerun with --apply to compact this exact scope\n");
    }
    out
}

fn print_write(r: &scratchpad::WriteResult) {
    let flag = match (r.over_cap, r.data_loss, r.consolidated) {
        (true, true, _) => " (over cap; destructive trim occurred)",
        (true, false, true) => " (still over cap after consolidation)",
        (true, false, false) => " (over cap)",
        (false, true, _) => " (destructive trim occurred)",
        (false, false, true) => " (consolidated)",
        (false, false, false) => "",
    };
    println!(
        "{CYAN}✓ {}:{} / {} — {}/{} bytes — {}{}{RESET}",
        r.scope_type, r.scope_key, r.block, r.bytes_used, r.cap_bytes, r.status, flag
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_result_render_reports_scope_flags_and_durable_evidence() {
        let result = scratchpad::MemoryCompactResult {
            scope_type: "project".to_string(),
            scope_key: "forge-fleet".to_string(),
            before_bytes: 8_000,
            after_bytes: 6_000,
            cap_bytes: 6_144,
            eligible_blocks: vec![scratchpad::CompactEligibleBlock {
                block: "scratch".to_string(),
                bytes: 2_000,
                hard_trim_eligible: true,
            }],
            applied: true,
            changed: true,
            data_loss: false,
            still_over_cap: false,
            eviction_evidence: 4,
            brain_evidence: 3,
            evictions_created: 1,
            brain_candidates_created: 1,
            status: "compacted".to_string(),
        };
        let rendered = render_compact_result(&result);
        assert!(rendered.contains("scope: project:forge-fleet"));
        assert!(rendered.contains("bytes: 8000 -> 6000 / 6144 cap"));
        assert!(rendered.contains("scratch: 2000 bytes"));
        assert!(
            rendered.contains("status=compacted changed=true data_loss=false still_over_cap=false")
        );
        assert!(rendered.contains("evictions=4 (+1), Brain candidates=3 (+1)"));
    }
}
