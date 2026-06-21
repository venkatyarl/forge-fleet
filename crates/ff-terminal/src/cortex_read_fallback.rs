use crate::{CYAN, RESET};
use anyhow::{Result, anyhow};
use ff_brain::{cortex, mirror};
use sqlx::PgPool;

const OFFLINE_NOTICE: &str = "(offline: using local cortex-cache snapshot)";

pub async fn handle_top_command(cmd: crate::top_cortex_cmd::TopCortexCommand) -> Result<()> {
    match cmd {
        crate::top_cortex_cmd::TopCortexCommand::Callers {
            symbol,
            corpus,
            min_confidence,
            format,
        } => {
            let corpus = corpus.unwrap_or_else(crate::top_cortex_cmd::cwd_slug);
            let rows = callers(&corpus, &symbol, min_confidence).await?;
            print_symbols(&rows, format.as_str(), &format!("callers of {symbol}"));
        }
        crate::top_cortex_cmd::TopCortexCommand::Callees {
            symbol,
            corpus,
            min_confidence,
            format,
        } => {
            let corpus = corpus.unwrap_or_else(crate::top_cortex_cmd::cwd_slug);
            let rows = callees(&corpus, &symbol, min_confidence).await?;
            print_symbols(&rows, format.as_str(), &format!("callees of {symbol}"));
        }
        crate::top_cortex_cmd::TopCortexCommand::Find {
            query,
            corpus,
            semantic: false,
            limit,
            kind,
            format,
        } => {
            let corpus = corpus.unwrap_or_else(crate::top_cortex_cmd::cwd_slug);
            let hits = find_symbols(&corpus, &query, limit, kind.as_deref()).await?;
            print_hits(&hits, format.as_str(), &query, &corpus);
            if hits.is_empty() {
                std::process::exit(1);
            }
        }
        _ => unreachable!("non-fallback cortex command dispatched to read fallback handler"),
    }
    Ok(())
}

async fn callers(
    corpus: &str,
    symbol: &str,
    min_confidence: f32,
) -> Result<Vec<cortex::SymbolRef>> {
    match open_pg_pool().await {
        Ok(pool) => match cortex::callers(&pool, corpus, symbol, min_confidence).await {
            Ok(value) => Ok(value),
            Err(err) if is_connection_error(&err) => {
                run_fallback(corpus, |conn| {
                    cortex::fallback::callers(conn, corpus, symbol, min_confidence)
                })
                .await
            }
            Err(err) => Err(err),
        },
        Err(err) if is_connection_error(&err) => {
            run_fallback(corpus, |conn| {
                cortex::fallback::callers(conn, corpus, symbol, min_confidence)
            })
            .await
        }
        Err(err) => Err(err),
    }
}

async fn callees(
    corpus: &str,
    symbol: &str,
    min_confidence: f32,
) -> Result<Vec<cortex::SymbolRef>> {
    match open_pg_pool().await {
        Ok(pool) => match cortex::callees(&pool, corpus, symbol, min_confidence).await {
            Ok(value) => Ok(value),
            Err(err) if is_connection_error(&err) => {
                run_fallback(corpus, |conn| {
                    cortex::fallback::callees(conn, corpus, symbol, min_confidence)
                })
                .await
            }
            Err(err) => Err(err),
        },
        Err(err) if is_connection_error(&err) => {
            run_fallback(corpus, |conn| {
                cortex::fallback::callees(conn, corpus, symbol, min_confidence)
            })
            .await
        }
        Err(err) => Err(err),
    }
}

async fn find_symbols(
    corpus: &str,
    query: &str,
    limit: i64,
    kind: Option<&str>,
) -> Result<Vec<cortex::SymbolHit>> {
    match open_pg_pool().await {
        Ok(pool) => match cortex::find_symbols(&pool, corpus, query, limit, kind).await {
            Ok(value) => Ok(value),
            Err(err) if is_connection_error(&err) => {
                run_fallback(corpus, |conn| {
                    cortex::fallback::find_symbols(conn, corpus, query, limit, kind)
                })
                .await
            }
            Err(err) => Err(err),
        },
        Err(err) if is_connection_error(&err) => {
            run_fallback(corpus, |conn| {
                cortex::fallback::find_symbols(conn, corpus, query, limit, kind)
            })
            .await
        }
        Err(err) => Err(err),
    }
}

async fn open_pg_pool() -> Result<PgPool> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow!("run_postgres_migrations: {e}"))?;
    Ok(pool)
}

async fn run_fallback<T, F>(corpus: &str, query: F) -> Result<T>
where
    F: FnOnce(&rusqlite::Connection) -> Result<T>,
{
    let conn = mirror::open_fallback(corpus).await.ok_or_else(|| {
        anyhow!("Postgres is unreachable and no cortex-cache snapshot exists for corpus '{corpus}'")
    })?;
    println!("{OFFLINE_NOTICE}");
    query(&conn)
}

fn is_connection_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(sqlx_err) = cause.downcast_ref::<sqlx::Error>() {
            return matches!(
                sqlx_err,
                sqlx::Error::Io(_)
                    | sqlx::Error::Tls(_)
                    | sqlx::Error::Protocol(_)
                    | sqlx::Error::PoolTimedOut
                    | sqlx::Error::PoolClosed
                    | sqlx::Error::WorkerCrashed
            );
        }
    }
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("connect postgres")
        || msg.contains("connection refused")
        || msg.contains("error communicating with database")
        || msg.contains("pool timed out")
        || msg.contains("closed pool")
        || msg.contains("no route to host")
        || msg.contains("operation timed out")
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
            println!("{CYAN}{} - {} result(s):{RESET}", label, rows.len());
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

fn print_hits(hits: &[cortex::SymbolHit], format: &str, query: &str, corpus: &str) {
    match format {
        "json" => {
            let v: Vec<_> = hits
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "id": h.id,
                        "qualified_name": h.qualified_name,
                        "node_type": h.node_type,
                        "file": h.file,
                        "start_line": h.start_line,
                        "fan_in": h.fan_in,
                        "score": h.score,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        }
        "names" => {
            for h in hits {
                println!("{}", h.qualified_name);
            }
        }
        _ => {
            println!(
                "{CYAN}▶ cortex find '{query}' in '{corpus}' - {} hit(s):{RESET}",
                hits.len()
            );
            if hits.is_empty() {
                println!("  (none - try a shorter fragment, or `ff cortex index` if stale)");
                return;
            }
            for h in hits {
                let kind = h.node_type.strip_prefix("code:").unwrap_or(&h.node_type);
                let loc = fmt_loc(h.file.as_deref(), h.start_line);
                println!(
                    "  {:<9} fanin={:<4} {}  ({loc})",
                    kind, h.fan_in, h.qualified_name
                );
            }
        }
    }
}

fn fmt_loc(file: Option<&str>, line: Option<i32>) -> String {
    match (file, line) {
        (Some(f), Some(l)) => format!("{f}:{l}"),
        (Some(f), None) => f.to_string(),
        _ => "?".to_string(),
    }
}
