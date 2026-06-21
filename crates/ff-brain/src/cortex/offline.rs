//! Read-only Cortex queries against the local SQLite mirror.
//!
//! The mirror is an exported snapshot of the Postgres graph, so these queries
//! intentionally mirror only the core navigation verbs needed during a database
//! outage.

use super::{SymbolHit, SymbolRef, kind_filter_types};
use anyhow::{Result, anyhow};
use rusqlite::{Connection, ToSql, params};
use std::collections::HashMap;
use uuid::Uuid;

pub fn find_symbols(
    conn: &Connection,
    corpus_slug: &str,
    query: &str,
    limit: i64,
    kind: Option<&str>,
) -> Result<Vec<SymbolHit>> {
    let limit = limit.clamp(1, 500);
    let kind_types = resolve_kind_filter(kind)?;
    let mut sql = String::from(
        r#"
        SELECT n.id, n.title, n.node_type, n.start_line,
               (SELECT COUNT(*) FROM edges e
                 WHERE e.edge_type = 'calls' AND e.dst_id = n.id) AS fan_in
          FROM nodes n
         WHERE n.project = ?1
           AND n.node_type LIKE 'code:%'
           AND instr(lower(n.title), lower(?2)) > 0
        "#,
    );
    if !kind_types.is_empty() {
        sql.push_str(" AND n.node_type IN (");
        sql.push_str(&vec!["?"; kind_types.len()].join(", "));
        sql.push(')');
    }
    sql.push_str(" ORDER BY fan_in DESC, n.title COLLATE BINARY LIMIT ?");

    let mut bind_values: Vec<&dyn ToSql> = vec![&corpus_slug, &query];
    for kind_type in &kind_types {
        bind_values.push(kind_type);
    }
    bind_values.push(&limit);

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(bind_values.as_slice(), |row| {
        Ok(SymbolHit {
            id: parse_uuid(row.get::<_, String>(0)?)?,
            qualified_name: row.get(1)?,
            node_type: row.get(2)?,
            file: None,
            start_line: row.get(3)?,
            fan_in: row.get(4)?,
            score: None,
        })
    })?;
    let mut hits = collect_rows(rows)?;
    resolve_hit_files(conn, &mut hits)?;
    Ok(hits)
}

pub fn callers(
    conn: &Connection,
    corpus_slug: &str,
    sel: &str,
    min_confidence: f32,
) -> Result<Vec<SymbolRef>> {
    let targets = resolve_symbol(conn, corpus_slug, sel)?;
    if targets.is_empty() {
        return Err(no_symbol_error(sel, corpus_slug));
    }
    let ids: Vec<String> = targets.iter().map(|t| t.id.to_string()).collect();
    let mut out = refs_for_edge_side(conn, &ids, "dst_id", "src_id", min_confidence)?;
    resolve_ref_files(conn, &mut out)?;
    Ok(out)
}

pub fn callees(
    conn: &Connection,
    corpus_slug: &str,
    sel: &str,
    min_confidence: f32,
) -> Result<Vec<SymbolRef>> {
    let srcs = resolve_symbol(conn, corpus_slug, sel)?;
    if srcs.is_empty() {
        return Err(no_symbol_error(sel, corpus_slug));
    }
    let ids: Vec<String> = srcs.iter().map(|t| t.id.to_string()).collect();
    let mut out = refs_for_edge_side(conn, &ids, "src_id", "dst_id", min_confidence)?;
    resolve_ref_files(conn, &mut out)?;
    Ok(out)
}

fn resolve_symbol(conn: &Connection, corpus_slug: &str, sel: &str) -> Result<Vec<SymbolRef>> {
    let exact_path = format!("code://{corpus_slug}/{sel}");
    let suffix = format!("%::{sel}");
    let mut stmt = conn.prepare(
        r#"
        SELECT id, title, node_type
          FROM nodes
         WHERE project = ?1
           AND node_type LIKE 'code:%'
           AND (path = ?2 OR title = ?3 OR title LIKE ?4)
         ORDER BY title COLLATE BINARY
        "#,
    )?;
    let rows = stmt.query_map(params![corpus_slug, exact_path, sel, suffix], |row| {
        Ok(SymbolRef {
            id: parse_uuid(row.get::<_, String>(0)?)?,
            qualified_name: row.get(1)?,
            node_type: row.get(2)?,
            file: None,
            start_line: None,
        })
    })?;
    collect_rows(rows)
}

fn refs_for_edge_side(
    conn: &Connection,
    ids: &[String],
    match_column: &str,
    ref_column: &str,
    min_confidence: f32,
) -> Result<Vec<SymbolRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        r#"
        SELECT DISTINCT n.id, n.title, n.node_type, n.start_line
          FROM edges e
          JOIN nodes n ON n.id = e.{ref_column}
         WHERE e.edge_type = 'calls'
           AND e.{match_column} IN ({placeholders})
           AND e.confidence >= ?
         ORDER BY n.title COLLATE BINARY
        "#
    );
    let mut bind_values: Vec<&dyn ToSql> = ids.iter().map(|id| id as &dyn ToSql).collect();
    bind_values.push(&min_confidence);

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(bind_values.as_slice(), |row| {
        Ok(SymbolRef {
            id: parse_uuid(row.get::<_, String>(0)?)?,
            qualified_name: row.get(1)?,
            node_type: row.get(2)?,
            file: None,
            start_line: row.get(3)?,
        })
    })?;
    collect_rows(rows)
}

fn resolve_hit_files(conn: &Connection, hits: &mut [SymbolHit]) -> Result<()> {
    let ids: Vec<Uuid> = hits.iter().map(|h| h.id).collect();
    let by_leaf = owning_files(conn, &ids)?;
    for hit in hits.iter_mut() {
        hit.file = by_leaf.get(&hit.id).cloned();
    }
    Ok(())
}

fn resolve_ref_files(conn: &Connection, refs: &mut [SymbolRef]) -> Result<()> {
    let ids: Vec<Uuid> = refs.iter().map(|r| r.id).collect();
    let by_leaf = owning_files(conn, &ids)?;
    for r in refs.iter_mut() {
        r.file = by_leaf.get(&r.id).cloned();
    }
    Ok(())
}

fn owning_files(conn: &Connection, ids: &[Uuid]) -> Result<HashMap<Uuid, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let id_strings: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    let placeholders = vec!["?"; id_strings.len()].join(", ");
    let sql = format!(
        r#"
        WITH RECURSIVE up AS (
            SELECT e.src_id AS anc, e.dst_id AS leaf
              FROM edges e
             WHERE e.edge_type = 'contains' AND e.dst_id IN ({placeholders})
            UNION
            SELECT e.src_id, up.leaf
              FROM edges e
              JOIN up ON e.dst_id = up.anc
             WHERE e.edge_type = 'contains'
        )
        SELECT up.leaf AS leaf, n.path AS path
          FROM up JOIN nodes n ON n.id = up.anc
         WHERE n.node_type = 'content:file'
        "#
    );
    let bind_values: Vec<&dyn ToSql> = id_strings.iter().map(|id| id as &dyn ToSql).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(bind_values.as_slice(), |row| {
        Ok((
            parse_uuid(row.get::<_, String>(0)?)?,
            row.get::<_, String>(1)?,
        ))
    })?;
    let mut by_leaf = HashMap::new();
    for row in rows {
        let (leaf, path) = row?;
        by_leaf.insert(leaf, path);
    }
    Ok(by_leaf)
}

fn resolve_kind_filter(kind: Option<&str>) -> Result<Vec<String>> {
    match kind {
        None => Ok(Vec::new()),
        Some(k) => match kind_filter_types(k) {
            Some(types) => Ok(types.into_iter().map(str::to_string).collect()),
            None => Err(anyhow!(
                "unknown --kind '{k}' (expected one of: function, struct, enum, trait, \
                 impl, mod, class, interface, type)"
            )),
        },
    }
}

fn no_symbol_error(sel: &str, corpus_slug: &str) -> anyhow::Error {
    anyhow!(
        "no code symbol matching '{sel}' in corpus '{corpus_slug}' (run `ff cortex find {sel}`)"
    )
}

fn parse_uuid(text: String) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(&text).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
