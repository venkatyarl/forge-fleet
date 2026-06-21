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
        crate::CortexCommand::IngestFleet => {
            println!("{CYAN}\u{25b6} Cortex ingesting fleet topology\u{2026}{RESET}");
            let touched = cortex::ingest_fleet::ingest_fleet(pool).await?;
            let counts = cortex::ingest_fleet::fleet_counts(pool).await?;
            println!("  upsert attempts:     {touched}");
            println!("  fleet nodes:         {}", counts.nodes());
            println!("    computers:         {}", counts.computers);
            println!("    models:            {}", counts.models);
            println!("    deployments:       {}", counts.deployments);
            println!("  fleet edges:         {}", counts.edges());
            println!("    runs_on:           {}", counts.runs_on_edges);
            println!("    serves_model:      {}", counts.serves_model_edges);
            println!("{CYAN}\u{2713} Done{RESET}");
        }
        crate::CortexCommand::Entities { corpus } => {
            println!("{CYAN}\u{25b6} Cortex deriving business entities\u{2026}{RESET}");
            let counts = cortex::ingest_biz::ingest_biz(pool, corpus.as_deref()).await?;
            let rows = cortex::ingest_biz::list_entities(pool, corpus.as_deref()).await?;
            println!("  upsert attempts:     {}", counts.upsert_attempts);
            println!("  biz entities:        {}", counts.entities);
            println!("  relates_to edges:    {}", counts.relates_to_edges);
            if rows.is_empty() {
                println!(
                    "no biz:entity nodes found (run `ff cortex index` for a corpus with DB schema?)"
                );
            } else {
                println!(
                    "  {:<28} {:<36} {:>10}  path",
                    "corpus", "entity", "relates_to"
                );
                for row in rows {
                    println!(
                        "  {:<28} {:<36} {:>10}  {}",
                        truncate(&row.corpus, 28),
                        truncate(&row.title, 36),
                        row.relates_to_count,
                        row.path
                    );
                }
            }
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
        crate::CortexCommand::Config { corpus, format } => {
            let rows = cortex::config(pool, corpus.as_deref()).await?;
            print_config(&rows, format.as_str());
        }
        crate::CortexCommand::ConfigKey {
            name,
            corpus,
            format,
        } => {
            let rows = cortex::config_key(pool, corpus.as_deref(), &name).await?;
            print_config_key(&rows, format.as_str(), &name);
        }
        crate::CortexCommand::Topics { corpus, format } => {
            let rows = cortex::topics(pool, corpus.as_deref()).await?;
            print_topics(&rows, format.as_str());
        }
        crate::CortexCommand::Topic {
            subject,
            corpus,
            format,
        } => {
            let rows = cortex::topic(pool, corpus.as_deref(), &subject).await?;
            print_topic(&rows, format.as_str(), &subject);
        }
        crate::CortexCommand::Gates { corpus, format } => {
            let report = cortex::security::gates(pool, corpus.as_deref()).await?;
            print_security_gates(&report, format.as_str());
        }
        crate::CortexCommand::Guards {
            corpus,
            symbol,
            format,
        } => {
            let rows = cortex::security::guards(pool, &corpus, &symbol).await?;
            print_security_guards(&rows, format.as_str(), &symbol);
        }
        crate::CortexCommand::Endpoints {
            path,
            corpus,
            format,
        } => {
            cortex::routes::print_endpoints_command(
                pool,
                corpus.as_deref(),
                path.as_deref(),
                format.as_str(),
            )
            .await?;
        }
        crate::CortexCommand::Errors { corpus, format } => {
            let rows = cortex::observ::errors(pool, corpus.as_deref()).await?;
            print_errors(&rows, format.as_str());
        }
        crate::CortexCommand::Logs { corpus, format } => {
            let rows = cortex::observ::logs(pool, corpus.as_deref()).await?;
            print_logs(&rows, format.as_str());
        }
        crate::CortexCommand::Owners {
            name,
            corpus,
            format,
        } => {
            if let Some(name) = name {
                let rows = cortex::owners::owner_files(pool, corpus.as_deref(), &name).await?;
                print_owner_files(&rows, format.as_str(), &name);
            } else {
                let rows = cortex::owners::owners(pool, corpus.as_deref()).await?;
                print_owners(&rows, format.as_str());
            }
        }
        crate::CortexCommand::Features { corpus, format } => {
            let rows = cortex::product::features(pool, corpus.as_deref()).await?;
            print_features(&rows, format.as_str());
        }
    }
    Ok(())
}

fn print_features(rows: &[cortex::product::FeatureRow], format: &str) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
            );
        }
        "names" => {
            for row in rows {
                println!("{}", row.feature);
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no product:feature nodes in cortex (run `ff cortex index`?)");
                return;
            }
            let implemented = rows.iter().filter(|row| row.has_implements()).count();
            println!(
                "{CYAN}\u{25b6} cortex features — {} feature(s), {} implemented:{RESET}",
                rows.len(),
                implemented
            );
            println!("  {:<32}  {:<11}  implements", "feature", "corpus");
            for row in rows {
                println!(
                    "  {:<32}  {:<11}  {}",
                    truncate(&row.feature, 32),
                    truncate(&row.corpus, 11),
                    row.implements.as_deref().unwrap_or("-")
                );
            }
        }
    }
}

fn print_owners(rows: &[cortex::owners::OwnerSummary], format: &str) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
            );
        }
        "names" => {
            for row in rows {
                println!("{}", row.name);
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no person:dev ownership nodes in cortex (run `ff cortex index`?)");
                return;
            }
            println!(
                "{CYAN}\u{25b6} cortex owners — {} owner(s):{RESET}",
                rows.len()
            );
            println!("  {:<36} {:>7}  corpus", "owner", "files");
            for row in rows {
                println!(
                    "  {:<36} {:>7}  {}",
                    truncate(&row.name, 36),
                    row.file_count,
                    row.corpus
                );
            }
        }
    }
}

fn print_owner_files(rows: &[cortex::owners::OwnedFile], format: &str, name: &str) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
            );
        }
        "names" => {
            for row in rows {
                println!("{}", row.path);
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no owned files for '{name}' in cortex (run `ff cortex index`?)");
                return;
            }
            println!(
                "{CYAN}\u{25b6} cortex owner '{name}' — {} file(s):{RESET}",
                rows.len()
            );
            for row in rows {
                println!(
                    "  {}  ({}, confidence {:.2})",
                    row.path, row.corpus, row.confidence
                );
            }
        }
    }
}

fn print_errors(rows: &[cortex::observ::ErrorType], format: &str) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
            );
        }
        "names" => {
            for row in rows {
                println!("{}", row.name);
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no error:type nodes in cortex (run `ff cortex index`?)");
                return;
            }
            println!(
                "{CYAN}\u{25b6} cortex errors — {} error type(s):{RESET}",
                rows.len()
            );
            println!("  {:<48} corpus", "type");
            for row in rows {
                println!("  {:<48} {}", truncate(&row.name, 48), row.corpus);
            }
        }
    }
}

fn print_logs(rows: &[cortex::observ::LogLevelSummary], format: &str) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
            );
        }
        "names" => {
            for row in rows {
                println!("{}", row.level);
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no obs:level nodes in cortex (run `ff cortex index`?)");
                return;
            }
            println!("{CYAN}\u{25b6} cortex logs:{RESET}");
            println!("  {:<8} {:>5}  corpus", "level", "emits");
            for row in rows {
                println!("  {:<8} {:>5}  {}", row.level, row.emits, row.corpus);
            }
            let error_functions = rows
                .iter()
                .flat_map(|row| row.error_functions.iter())
                .collect::<Vec<_>>();
            if !error_functions.is_empty() {
                println!("  error-level emitters:");
                for function in error_functions {
                    println!("    {function}");
                }
            }
        }
    }
}

fn print_topics(rows: &[cortex::EventTopicSummary], format: &str) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
            );
        }
        "names" => {
            for row in rows {
                println!("{}", row.subject);
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no event:topic nodes in cortex (run `ff cortex index`?)");
                return;
            }
            println!(
                "{CYAN}\u{25b6} cortex topics — {} topic(s):{RESET}",
                rows.len()
            );
            println!("  {:<56} {:>4} {:>4}  corpus", "subject", "pub", "sub");
            for row in rows {
                let marker = if row.one_sided { " !" } else { "  " };
                println!(
                    "{marker}{:<56} {:>4} {:>4}  {}",
                    truncate(&row.subject, 56),
                    row.publishers,
                    row.subscribers,
                    row.corpus
                );
            }
            if rows.iter().any(|r| r.one_sided) {
                println!(
                    "  ! one-sided topic: publishers without subscribers or subscribers without publishers"
                );
            }
        }
    }
}

fn print_topic(rows: &[cortex::EventTopicDetail], format: &str, subject: &str) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
            );
        }
        "names" => {
            for row in rows {
                for endpoint in row.publishers.iter().chain(row.subscribers.iter()) {
                    println!("{}", endpoint.qualified_name);
                }
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no event:topic '{subject}' in cortex (run `ff cortex index`?)");
                return;
            }
            for row in rows {
                println!(
                    "{CYAN}\u{25b6} cortex topic '{}' ({}){RESET}",
                    row.subject, row.corpus
                );
                if row.one_sided {
                    println!(
                        "  ! one-sided topic: publishers without subscribers or subscribers without publishers"
                    );
                }
                println!("  publishers:");
                if row.publishers.is_empty() {
                    println!("    -");
                } else {
                    for endpoint in &row.publishers {
                        print_endpoint(endpoint);
                    }
                }
                println!("  subscribers:");
                if row.subscribers.is_empty() {
                    println!("    -");
                } else {
                    for endpoint in &row.subscribers {
                        print_endpoint(endpoint);
                    }
                }
            }
        }
    }
}

fn print_endpoint(endpoint: &cortex::EventEndpoint) {
    let method = endpoint.method.as_deref().unwrap_or("-");
    println!(
        "    {}  ({method}, confidence {:.2})",
        endpoint.qualified_name, endpoint.confidence
    );
}

fn print_security_gates(report: &cortex::security::SecurityGatesReport, format: &str) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_string())
            );
        }
        "names" => {
            for row in &report.gates {
                println!("{}", row.gate);
            }
            for handler in &report.unguarded_handlers {
                println!("unguarded:{}", handler.qualified_name);
            }
        }
        _ => {
            if report.gates.is_empty() {
                println!("no security:gate nodes in cortex (run `ff cortex index`?)");
            } else {
                println!(
                    "{CYAN}\u{25b6} cortex gates — {} gate(s):{RESET}",
                    report.gates.len()
                );
                println!("  {:<28} {:>5}  corpus", "gate", "fns");
                for row in &report.gates {
                    println!(
                        "  {:<28} {:>5}  {}",
                        truncate(&row.gate, 28),
                        row.protected_functions,
                        row.corpus
                    );
                }
            }

            if !report.unguarded_handlers.is_empty() {
                println!("  candidate unauthenticated handlers:");
                for handler in &report.unguarded_handlers {
                    let line = handler
                        .start_line
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "-".to_string());
                    println!("    {}  ({}:{line})", handler.qualified_name, handler.path);
                }
            }
        }
    }
}

fn print_security_guards(
    rows: &[cortex::security::SecurityGuardDetail],
    format: &str,
    symbol: &str,
) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
            );
        }
        "names" => {
            for row in rows {
                for gate in &row.gates {
                    println!("{}", gate.gate);
                }
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no guards for '{symbol}' in cortex (run `ff cortex index`?)");
                return;
            }
            for row in rows {
                println!(
                    "{CYAN}\u{25b6} cortex guards for '{}' ({}){RESET}",
                    row.symbol, row.corpus
                );
                if row.gates.is_empty() {
                    println!("  -");
                } else {
                    for gate in &row.gates {
                        let method = gate.method.as_deref().unwrap_or("-");
                        println!(
                            "  {}  ({method}, confidence {:.2})",
                            gate.gate, gate.confidence
                        );
                    }
                }
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out = s.chars().take(max.saturating_sub(1)).collect::<String>();
    out.push('…');
    out
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

fn print_config(rows: &[cortex::ConfigSummary], format: &str) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
            );
        }
        "names" => {
            for row in rows {
                println!("{}", row.key);
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no config:* nodes in cortex (run `ff cortex index`?)");
                return;
            }
            println!(
                "{CYAN}\u{25b6} cortex config — {} key(s):{RESET}",
                rows.len()
            );
            println!("  {:<14} {:<56} {:>7}  corpus", "type", "key", "readers");
            for row in rows {
                println!(
                    "  {:<14} {:<56} {:>7}  {}",
                    row.node_type,
                    truncate(&row.key, 56),
                    row.readers,
                    row.corpus
                );
            }
        }
    }
}

fn print_config_key(rows: &[cortex::ConfigKeyDetail], format: &str, name: &str) {
    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
            );
        }
        "names" => {
            for row in rows {
                for reader in &row.readers {
                    println!("{}", reader.qualified_name);
                }
            }
        }
        _ => {
            if rows.is_empty() {
                println!("no config key '{name}' in cortex (run `ff cortex index`?)");
                return;
            }
            for row in rows {
                println!(
                    "{CYAN}\u{25b6} cortex config-key '{}' ({}, {}){RESET}",
                    row.key, row.node_type, row.corpus
                );
                if row.readers.is_empty() {
                    println!("  readers: -");
                    continue;
                }
                println!("  readers:");
                for reader in &row.readers {
                    let method = reader.method.as_deref().unwrap_or("-");
                    println!(
                        "    {}  ({method}, confidence {:.2})",
                        reader.qualified_name, reader.confidence
                    );
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

pub(crate) fn print_symbols(rows: &[cortex::SymbolRef], format: &str, label: &str) {
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
