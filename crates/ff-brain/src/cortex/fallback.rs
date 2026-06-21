use super::{SymbolHit, SymbolRef, escape_like, kind_filter_types};
use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

pub fn find_symbols(
    conn: &Connection,
    corpus_slug: &str,
    query: &str,
    limit: i64,
    kind: Option<&str>,
) -> Result<Vec<SymbolHit>> {
    let pattern = format!("%{}%", escape_like(query));
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
           AND n.title LIKE ?2 ESCAPE '\'
        "#,
    );
    if !kind_types.is_empty() {
        sql.push_str(" AND n.node_type IN (");
        push_placeholders(&mut sql, 4, kind_types.len());
        sql.push(')');
    }
    sql.push_str(" ORDER BY fan_in DESC, n.title COLLATE BINARY LIMIT ?3");

    let mut stmt = conn.prepare(&sql)?;
    let mut values: Vec<&dyn rusqlite::ToSql> = vec![&corpus_slug, &pattern, &limit];
    for kind in &kind_types {
        values.push(kind);
    }
    let rows = stmt.query_map(values.as_slice(), |row| {
        Ok(SymbolHit {
            id: uuid_from_row(row, 0)?,
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
    let mut out = callers_of_ids(conn, &ids, min_confidence)?;
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
    let mut sql = String::from(
        r#"
        SELECT DISTINCT n.id, n.title, n.node_type, n.start_line
          FROM edges e
          JOIN nodes n ON n.id = e.dst_id
         WHERE e.edge_type = 'calls'
           AND e.confidence >= ?1
           AND e.src_id IN (
        "#,
    );
    push_placeholders(&mut sql, 2, ids.len());
    sql.push_str(") ORDER BY n.title COLLATE BINARY");

    let mut stmt = conn.prepare(&sql)?;
    let mut values: Vec<&dyn rusqlite::ToSql> = vec![&min_confidence];
    for id in &ids {
        values.push(id);
    }
    let rows = stmt.query_map(values.as_slice(), symbol_ref_from_row)?;
    let mut out = collect_rows(rows)?;
    resolve_ref_files(conn, &mut out)?;
    Ok(out)
}

fn resolve_symbol(conn: &Connection, corpus_slug: &str, sel: &str) -> Result<Vec<SymbolRef>> {
    let exact_path = format!("code://{corpus_slug}/{sel}");
    let suffix = format!("%::{sel}");
    let mut stmt = conn.prepare(
        r#"
        SELECT id, title, node_type, start_line
          FROM nodes
         WHERE project = ?1
           AND node_type LIKE 'code:%'
           AND (path = ?2 OR title = ?3 OR title LIKE ?4)
         ORDER BY title COLLATE BINARY
        "#,
    )?;
    let rows = stmt.query_map(params![corpus_slug, exact_path, sel, suffix], |row| {
        Ok(SymbolRef {
            id: uuid_from_row(row, 0)?,
            qualified_name: row.get(1)?,
            node_type: row.get(2)?,
            file: None,
            start_line: row.get(3)?,
        })
    })?;
    collect_rows(rows)
}

fn callers_of_ids(
    conn: &Connection,
    ids: &[String],
    min_confidence: f32,
) -> Result<Vec<SymbolRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = String::from(
        r#"
        SELECT DISTINCT n.id, n.title, n.node_type, n.start_line
          FROM edges e
          JOIN nodes n ON n.id = e.src_id
         WHERE e.edge_type = 'calls'
           AND e.confidence >= ?1
           AND e.dst_id IN (
        "#,
    );
    push_placeholders(&mut sql, 2, ids.len());
    sql.push_str(") ORDER BY n.title COLLATE BINARY");

    let mut stmt = conn.prepare(&sql)?;
    let mut values: Vec<&dyn rusqlite::ToSql> = vec![&min_confidence];
    for id in ids {
        values.push(id);
    }
    let rows = stmt.query_map(values.as_slice(), symbol_ref_from_row)?;
    collect_rows(rows)
}

fn resolve_hit_files(conn: &Connection, hits: &mut [SymbolHit]) -> Result<()> {
    for hit in hits {
        hit.file = owning_file(conn, &hit.id.to_string())?;
    }
    Ok(())
}

fn resolve_ref_files(conn: &Connection, refs: &mut [SymbolRef]) -> Result<()> {
    for r in refs {
        r.file = owning_file(conn, &r.id.to_string())?;
    }
    Ok(())
}

fn owning_file(conn: &Connection, id: &str) -> Result<Option<String>> {
    conn.query_row(
        r#"
        WITH RECURSIVE up(anc, leaf) AS (
            SELECT e.src_id, e.dst_id
              FROM edges e
             WHERE e.edge_type = 'contains' AND e.dst_id = ?1
            UNION
            SELECT e.src_id, up.leaf
              FROM edges e
              JOIN up ON e.dst_id = up.anc
             WHERE e.edge_type = 'contains'
        )
        SELECT n.path
          FROM up
          JOIN nodes n ON n.id = up.anc
         WHERE n.node_type = 'content:file'
         LIMIT 1
        "#,
        params![id],
        |row| row.get(0),
    )
    .optional()
    .context("resolve owning file from sqlite cortex fallback")
}

fn resolve_kind_filter(kind: Option<&str>) -> Result<Vec<String>> {
    match kind {
        None => Ok(Vec::new()),
        Some(k) => match kind_filter_types(k) {
            Some(types) => Ok(types.iter().map(|s| s.to_string()).collect()),
            None => anyhow::bail!(
                "unknown --kind '{k}' (expected one of: function, struct, enum, trait, \
                 impl, mod, class, interface, type)"
            ),
        },
    }
}

fn push_placeholders(sql: &mut String, start: usize, count: usize) {
    for i in 0..count {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
        sql.push_str(&(start + i).to_string());
    }
}

fn symbol_ref_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolRef> {
    Ok(SymbolRef {
        id: uuid_from_row(row, 0)?,
        qualified_name: row.get(1)?,
        node_type: row.get(2)?,
        file: None,
        start_line: row.get(3)?,
    })
}

fn uuid_from_row(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Uuid> {
    let s: String = row.get(idx)?;
    Uuid::parse_str(&s).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(err))
    })
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

fn no_symbol_error(sel: &str, corpus_slug: &str) -> anyhow::Error {
    anyhow!("no symbol matching '{sel}' in corpus '{corpus_slug}'")
}
