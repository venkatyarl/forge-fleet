//! `ff brain cortex …` + `ff brain callers/callees/impact …` handlers.
//!
//! Thin dispatch into ff_brain::cortex, reusing the cached fleet PgPool passed
//! in by brain_cmd (same pattern as corpus_cmd).

use crate::{CYAN, RESET};
use anyhow::Result;
use ff_brain::cortex;
use sqlx::PgPool;

pub async fn handle_cortex(pool: &PgPool, cmd: crate::CortexCommand) -> Result<()> {
    match cmd {
        crate::CortexCommand::Index { slug, lang } => {
            println!("{CYAN}\u{25b6} Cortex indexing corpus '{slug}' (lang={lang})\u{2026}{RESET}");
            let stats = cortex::index(pool, &slug, &lang).await?;
            println!("  files parsed:        {}", stats.files_parsed);
            println!("  symbols:             {}", stats.symbols);
            println!("  contains edges:      {}", stats.contains);
            println!("  import edges:        {}", stats.imports);
            println!("  calls (total):       {}", stats.calls_total);
            println!("  calls (resolved):    {}", stats.calls_resolved);
            println!("  inherited members:   {}", stats.inherited_memberships);
            println!("{CYAN}\u{2713} Done{RESET}");
        }
        crate::CortexCommand::Callers {
            corpus,
            symbol,
            format,
        } => {
            let rows = cortex::callers(pool, &corpus, &symbol).await?;
            print_symbols(&rows, &format, &format!("callers of {symbol}"));
        }
        crate::CortexCommand::Callees {
            corpus,
            symbol,
            format,
        } => {
            let rows = cortex::callees(pool, &corpus, &symbol).await?;
            print_symbols(&rows, &format, &format!("callees of {symbol}"));
        }
        crate::CortexCommand::Impact {
            corpus,
            symbol,
            max_depth,
            format,
        } => {
            let rows = cortex::impact(pool, &corpus, &symbol, max_depth).await?;
            print_symbols(
                &rows,
                &format,
                &format!("impact of {symbol} (depth {max_depth})"),
            );
        }
        crate::CortexCommand::Tests {
            corpus,
            symbol,
            max_depth,
            format,
        } => {
            let rows = cortex::tests_for(pool, &corpus, &symbol, max_depth).await?;
            print_tests(
                &rows,
                &format,
                &format!("tests covering {symbol} (depth {max_depth})"),
            );
        }
    }
    Ok(())
}

/// Render `ff cortex tests` hits. `table` shows depth + location + the test's
/// qualified name (nearest-first); `json` emits the full records; `names` lists
/// just the qualified names. An empty result is surfaced as a coverage gap.
fn print_tests(rows: &[cortex::TestHit], format: &str, label: &str) {
    match format {
        "json" => {
            let v: Vec<_> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "qualified_name": r.qualified_name,
                        "file": r.file,
                        "depth": r.depth,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        }
        "names" => {
            for r in rows {
                println!("{}", r.qualified_name);
            }
        }
        _ => {
            if rows.is_empty() {
                println!("{CYAN}{label} \u{2014} no covering tests found (coverage gap){RESET}");
                return;
            }
            println!("{CYAN}{} \u{2014} {} test(s):{RESET}", label, rows.len());
            for r in rows {
                let loc = r.file.as_deref().unwrap_or("?");
                println!("  [d{}] {}  ({})", r.depth, r.qualified_name, loc);
            }
        }
    }
}

fn print_symbols(rows: &[cortex::SymbolRef], format: &str, label: &str) {
    match format {
        "json" => {
            let v: Vec<_> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "qualified_name": r.qualified_name,
                        "node_type": r.node_type,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        }
        "names" => {
            for r in rows {
                println!("{}", r.qualified_name);
            }
        }
        _ => {
            println!("{CYAN}{} \u{2014} {} result(s):{RESET}", label, rows.len());
            for r in rows {
                println!("  [{}] {}", r.node_type, r.qualified_name);
            }
        }
    }
}
