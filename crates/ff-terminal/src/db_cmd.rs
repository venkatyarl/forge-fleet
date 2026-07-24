//! `ff db` — a read-only SQL escape hatch against the ForgeFleet Postgres.
//!
//! Why this exists: there was NO `ff` verb to run an ad-hoc read-only query,
//! so `ff db query "SELECT …"` (a perfectly natural thing for an operator or
//! either autopilot loop to type) fell through clap's unknown-subcommand arm
//! into the free-prompt LLM agent dispatcher — which, with no real DB tool,
//! HALLUCINATED a fake SQLite database and fabricated rows (observed
//! 2026-06-13: invented `~/.forgefleet/fleet.db` results when the real store
//! is Docker Postgres). Making `db` a real subcommand both (a) gives a safe
//! inspection tool and (b) closes that dangerous fall-through for the `db`
//! prefix.
//!
//! Safety: the statement runs inside a `READ ONLY` transaction (the server
//! rejects any write), AND it is wrapped in a subquery so a non-SELECT is a
//! syntax error and a data-modifying CTE is illegal at non-top-level. Two
//! independent layers — appropriate for an unattended tool.

use anyhow::{Context, Result};
use sqlx::{Column, Row};

pub async fn handle_db(cmd: crate::DbCommand) -> Result<()> {
    match cmd {
        crate::DbCommand::Query {
            sql,
            json,
            max_rows,
        } => query(&sql, json, max_rows).await,
        crate::DbCommand::Exec { sql, params } => exec(&sql, params).await,
    }
}

/// Convert CLI `?` placeholders to the numbered placeholders Postgres expects.
/// Question marks inside SQL string or identifier quotes remain literal.
fn postgres_placeholders(sql: &str) -> (String, usize) {
    let mut converted = String::with_capacity(sql.len());
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut count = 0;
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double_quote => {
                converted.push(ch);
                if in_single_quote && chars.peek() == Some(&'\'') {
                    converted.push(chars.next().expect("peeked quote"));
                } else {
                    in_single_quote = !in_single_quote;
                }
            }
            '"' if !in_single_quote => {
                converted.push(ch);
                if in_double_quote && chars.peek() == Some(&'"') {
                    converted.push(chars.next().expect("peeked quote"));
                } else {
                    in_double_quote = !in_double_quote;
                }
            }
            '?' if !in_single_quote && !in_double_quote => {
                count += 1;
                converted.push('$');
                converted.push_str(&count.to_string());
            }
            _ => converted.push(ch),
        }
    }

    (converted, count)
}

fn parse_param(value: String) -> serde_json::Value {
    serde_json::from_str::<serde_json::Number>(&value)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::String(value))
}

async fn exec(raw_sql: &str, raw_params: Vec<String>) -> Result<()> {
    let trimmed = raw_sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty statement — usage: ff db exec \"UPDATE … SET value = ?\" value");
    }

    let (sql, placeholder_count) = postgres_placeholders(trimmed);
    if placeholder_count != raw_params.len() {
        anyhow::bail!(
            "statement has {placeholder_count} placeholder(s), but {} parameter(s) were provided",
            raw_params.len()
        );
    }

    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    let rows = ff_db::db_exec(
        &pool,
        &sql,
        raw_params.into_iter().map(parse_param).collect(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("exec failed: {e}"))?;

    println!("({rows} row{} affected)", if rows == 1 { "" } else { "s" });
    Ok(())
}

/// Build the wrapped, row-capped statement. Pure so it can be unit-tested.
///
/// - `to_jsonb(__ffq)` lets us read every column value uniformly regardless of
///   its Postgres type (no per-type sqlx decode).
/// - `__ffq.*` carries the real column metadata so we can preserve the SELECT
///   column order (serde_json alone alphabetises keys).
/// - `LIMIT cap+1` so the caller can detect truncation.
fn build_wrapped(sql: &str, cap: usize) -> String {
    let inner = sql.trim().trim_end_matches(';').trim();
    format!(
        "SELECT to_jsonb(__ffq) AS __ffq_row, __ffq.* FROM ( {inner} ) AS __ffq LIMIT {}",
        cap + 1
    )
}

/// Render one JSON cell value for table output (strings unquoted, null blank,
/// everything else compact). Pure for unit tests.
fn cell_str(v: Option<&serde_json::Value>) -> String {
    match v {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

async fn query(raw_sql: &str, json: bool, max_rows: usize) -> Result<()> {
    let trimmed = raw_sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty query — usage: ff db query \"SELECT …\"");
    }
    let cap = max_rows.max(1);
    let wrapped = build_wrapped(raw_sql, cap);

    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;

    // READ ONLY transaction — defence in depth beyond the subquery wrapping.
    let mut tx = pool.begin().await.context("begin transaction")?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .context("set transaction read only")?;
    let rows = sqlx::query(&wrapped)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
    drop(tx); // rollback — nothing to commit on a read-only tx.

    let truncated = rows.len() > cap;
    let shown = &rows[..rows.len().min(cap)];

    if shown.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("(0 rows)");
        }
        return Ok(());
    }

    if json {
        let arr = serde_json::Value::Array(
            shown
                .iter()
                .filter_map(|r| r.try_get::<serde_json::Value, _>("__ffq_row").ok())
                .collect(),
        );
        println!("{}", serde_json::to_string_pretty(&arr)?);
        if truncated {
            eprintln!("… {cap}-row cap reached; more rows exist (raise with --max-rows).");
        }
        return Ok(());
    }

    // Column order from the result-set metadata (not the alphabetised jsonb).
    let cols: Vec<String> = shown[0]
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .filter(|n| n != "__ffq_row")
        .collect();
    let objs: Vec<serde_json::Map<String, serde_json::Value>> = shown
        .iter()
        .map(|r| {
            r.try_get::<serde_json::Value, _>("__ffq_row")
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default()
        })
        .collect();

    render_table(&cols, &objs);
    if truncated {
        eprintln!("… {cap}-row cap reached; more rows exist (raise with --max-rows).");
    }
    Ok(())
}

/// Aligned table, columns capped to keep the terminal readable.
fn render_table(cols: &[String], rows: &[serde_json::Map<String, serde_json::Value>]) {
    const MAX_W: usize = 60;
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|o| {
            cols.iter()
                .map(|c| {
                    let mut s = cell_str(o.get(c));
                    if s.contains('\n') {
                        s = s.replace('\n', " ⏎ ");
                    }
                    if s.chars().count() > MAX_W {
                        s = format!("{}…", s.chars().take(MAX_W - 1).collect::<String>());
                    }
                    s
                })
                .collect()
        })
        .collect();

    let mut widths: Vec<usize> = cols.iter().map(|c| c.chars().count()).collect();
    for row in &cells {
        for (i, s) in row.iter().enumerate() {
            widths[i] = widths[i].max(s.chars().count()).min(MAX_W);
        }
    }

    let header: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{:<w$}", c, w = widths[i]))
        .collect();
    println!("{}", header.join("  "));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  ")
    );
    for row in &cells {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{:<w$}", s, w = widths[i]))
            .collect();
        println!("{}", line.join("  "));
    }
    println!(
        "({} row{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_wrapped_strips_trailing_semicolon_and_caps() {
        let w = build_wrapped("SELECT 1;  ", 50);
        assert!(w.contains("FROM ( SELECT 1 ) AS __ffq"));
        assert!(w.ends_with("LIMIT 51"), "got: {w}");
        assert!(!w.contains(';'), "trailing ; must be stripped: {w}");
    }

    #[test]
    fn build_wrapped_preserves_inner_where_and_cte() {
        // Inner WITH stays inside the subquery (valid in PG); no semicolon leak.
        let w = build_wrapped("WITH x AS (SELECT 1 a) SELECT * FROM x", 10);
        assert!(w.contains("WITH x AS (SELECT 1 a) SELECT * FROM x"));
        assert!(w.ends_with("LIMIT 11"));
    }

    #[test]
    fn cell_str_renders_each_type() {
        assert_eq!(cell_str(None), "");
        assert_eq!(cell_str(Some(&json!(null))), "");
        assert_eq!(cell_str(Some(&json!("hi"))), "hi"); // unquoted
        assert_eq!(cell_str(Some(&json!(42))), "42");
        assert_eq!(cell_str(Some(&json!(true))), "true");
        // objects/arrays render compact
        assert_eq!(cell_str(Some(&json!({"k":1}))), "{\"k\":1}");
    }

    #[test]
    fn postgres_placeholders_skips_quoted_question_marks() {
        let (sql, count) =
            postgres_placeholders("UPDATE t SET value = ? WHERE note = '?' AND \"?\" = ?");
        assert_eq!(
            sql,
            "UPDATE t SET value = $1 WHERE note = '?' AND \"?\" = $2"
        );
        assert_eq!(count, 2);
    }

    #[test]
    fn parse_param_preserves_strings_and_parses_numbers() {
        assert_eq!(parse_param("42".into()), json!(42));
        assert_eq!(parse_param("-1.5".into()), json!(-1.5));
        assert_eq!(parse_param("worker-1".into()), json!("worker-1"));
    }
}
