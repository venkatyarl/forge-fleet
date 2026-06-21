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
            println!(
                "  calls (resolved):    {}  (extracted {} / inferred {})",
                stats.calls_resolved,
                stats.calls_resolved.saturating_sub(stats.calls_inferred),
                stats.calls_inferred,
            );
            println!("  inherited members:   {}", stats.inherited_memberships);
            println!("{CYAN}\u{2713} Done{RESET}");
        }
        crate::CortexCommand::Callers {
            corpus,
            symbol,
            min_confidence,
            format,
        } => {
            let rows = cortex::callers(pool, &corpus, &symbol, min_confidence).await?;
            print_symbols(&rows, format.as_str(), &format!("callers of {symbol}"));
        }
        crate::CortexCommand::Callees {
            corpus,
            symbol,
            min_confidence,
            format,
        } => {
            let rows = cortex::callees(pool, &corpus, &symbol, min_confidence).await?;
            print_symbols(&rows, format.as_str(), &format!("callees of {symbol}"));
        }
        crate::CortexCommand::Impact {
            corpus,
            symbol,
            max_depth,
            min_confidence,
            format,
        } => {
            let rows = cortex::impact(pool, &corpus, &symbol, max_depth, min_confidence).await?;
            print_symbols(
                &rows,
                format.as_str(),
                &format!("impact of {symbol} (depth {max_depth})"),
            );
        }
        crate::CortexCommand::Tests {
            corpus,
            symbol,
            max_depth,
            min_confidence,
            format,
        } => {
            let rows = cortex::tests_for(pool, &corpus, &symbol, max_depth, min_confidence).await?;
            print_tests(
                &rows,
                format.as_str(),
                &format!("tests covering {symbol} (depth {max_depth})"),
            );
        }
        crate::CortexCommand::Path {
            corpus,
            from,
            to,
            max_depth,
            min_confidence,
            format,
        } => {
            let path =
                cortex::call_path(pool, &corpus, &from, &to, max_depth, min_confidence).await?;
            print_path(path.as_deref(), format.as_str(), &from, &to, max_depth);
        }
        crate::CortexCommand::Field {
            field,
            corpus,
            format,
        } => {
            let rows = cortex::field(pool, corpus.as_deref(), &field).await?;
            print_fields(&rows, format.as_str(), &field);
        }
    }
    Ok(())
}

fn print_fields(rows: &[cortex::DbField], format: &str, field: &str) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
            );
        }
        "names" => {
            for row in rows {
                println!("{}", row.column);
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no db:column '{field}' in cortex (run `ff cortex index`?)");
                return;
            }
            println!(
                "{CYAN}\u{25b6} cortex field '{field}' — {} hit(s):{RESET}",
                rows.len()
            );
            for row in rows {
                let typ = row
                    .descriptor
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let nullable = row
                    .descriptor
                    .get("nullable")
                    .and_then(|v| v.as_bool())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let default = row
                    .descriptor
                    .get("default")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("-");
                let check = row
                    .descriptor
                    .get("check")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("-");
                println!("  {}", row.column);
                println!("    corpus:   {}", row.corpus);
                println!("    table:    {}", row.table);
                println!("    type:     {}", if typ.is_empty() { "-" } else { typ });
                println!("    nullable: {nullable}");
                println!("    default:  {default}");
                println!("    check:    {check}");
                if row.migrations.is_empty() {
                    println!("    migrations: -");
                } else {
                    let migrations = row
                        .migrations
                        .iter()
                        .map(|m| format!("{} {}", m.edge_type, m.title))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("    migrations: {migrations}");
                }
            }
        }
    }
}

/// Render `ff cortex path`. `table` shows the chain `from → … → to` with each
/// hop's location; `json` emits `{found, from, to, hops, path:[…]}`; `names`
/// lists the qualified names one per line (source first). No path is a
/// legitimate empty result (exit 0), not an error.
fn print_path(
    path: Option<&[cortex::SymbolRef]>,
    format: &str,
    from: &str,
    to: &str,
    depth: usize,
) {
    match format {
        "json" => {
            let nodes: Vec<_> = path
                .unwrap_or_default()
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "qualified_name": r.qualified_name,
                        "node_type": r.node_type,
                        "file": r.file,
                        "start_line": r.start_line,
                    })
                })
                .collect();
            let v = serde_json::json!({
                "from": from,
                "to": to,
                "found": path.is_some(),
                // hop count = edges traversed = nodes - 1 (0 for a same-node path).
                "hops": path.map(|p| p.len().saturating_sub(1)),
                "path": nodes,
            });
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        }
        "names" => {
            for r in path.unwrap_or_default() {
                println!("{}", r.qualified_name);
            }
        }
        _ => {
            let Some(p) = path else {
                println!("{CYAN}no call path from {from} to {to} within depth {depth}{RESET}");
                return;
            };
            println!(
                "{CYAN}call path {from} \u{2192} {to} \u{2014} {} hop(s):{RESET}",
                p.len().saturating_sub(1)
            );
            for (i, r) in p.iter().enumerate() {
                let arrow = if i == 0 { " " } else { "\u{2192}" };
                println!(
                    "  {arrow} {}  ({})",
                    r.qualified_name,
                    fmt_loc(r.file.as_deref(), r.start_line)
                );
            }
        }
    }
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
                        "start_line": r.start_line,
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
                println!(
                    "  [d{}] {}  ({})",
                    r.depth,
                    r.qualified_name,
                    fmt_loc(r.file.as_deref(), r.start_line)
                );
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
                        "file": r.file,
                        "start_line": r.start_line,
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
                println!(
                    "  [{}] {}  ({})",
                    r.node_type,
                    r.qualified_name,
                    fmt_loc(r.file.as_deref(), r.start_line)
                );
            }
        }
    }
}

/// Format a symbol's location as `file:line` (or `file`, or `?`) for the table
/// views of the relationship verbs — the actionable `path:line` form an agent or
/// editor can jump to, so `callers`/`callees`/`impact`/`tests` no longer need a
/// second `find`/`show` round-trip to locate each result.
fn fmt_loc(file: Option<&str>, line: Option<i32>) -> String {
    match (file, line) {
        (Some(f), Some(l)) => format!("{f}:{l}"),
        (Some(f), None) => f.to_string(),
        _ => "?".to_string(),
    }
}
