//! Cortex — native code-extraction lobe for the Brain faceted graph.
//!
//! Parses code files (Rust, TypeScript/TSX, JavaScript, Java) with the native
//! `tree-sitter` grammar crates — NO Python, NO external tool. For
//! each file already scanned by `corpus.rs` as a `content:file` brain_vault_nodes
//! row, Cortex extracts symbol nodes and call/import/contains edges into the V117
//! Brain tables (reused wholesale — NO new tables, NO new columns).
//!
//! NODE MODEL
//!   Code symbols are `brain_vault_nodes` rows with `node_type` in
//!   {code:function, code:struct, code:enum, code:trait, code:impl, code:mod,
//!    code:class, code:interface, code:import, code:extern}.
//!   Each symbol's `path` is the synthetic unique key
//!   `code://<corpus_slug>/<qualified_name>` and `title` holds the qualified name,
//!   so a symbol resolves by qualified name via the existing UNIQUE(path)
//!   constraint (no new column needed). `project` = corpus slug.
//!
//! EDGE MODEL (brain_vault_edges, provenance='cortex')
//!   - contains : file -> symbol, impl -> method, mod -> child
//!   - imports  : file -> code:import node holding the fully-qualified use target
//!   - calls    : caller-fn -> RESOLVED qualified callee (a real code:function
//!                when internal, else a code:extern placeholder on the same
//!                code:// path so callers_of still works)
//!
//! THE DIFFERENTIATOR — qualified call-path resolution (resolve_call):
//!   Each file derives its crate name (nearest Cargo.toml [package].name) + module
//!   segments (dir/file path under src), and builds a per-file `use` alias map
//!   handling `a::b::c`, `as`, brace groups `a::{b,c}`, and
//!   `use crate::m::{self,..}` (self binds the parent module). Leading `crate::`
//!   normalizes to the real crate name. Each call attributes to its innermost
//!   enclosing fn (byte span), and bare/self/super resolve against that caller's
//!   own module. Call shapes resolved:
//!     bare foo()          -> <caller-module>::foo
//!     self::foo()         -> <caller-module>::foo
//!     super::foo()        -> <parent-module>::foo
//!     m::foo()            -> alias-expanded if `m` is a use alias, else
//!                            <caller-module>::m::foo
//!     crate::a::b::foo()  -> <crate>::a::b::foo
//!     alias::foo()        -> <expanded-alias>::foo
//!     std/external        -> treated as already-qualified
//!   ERROR-node descent keeps functions after parse errors (else false self-edges).
//!
//! TYPESCRIPT / JAVASCRIPT (parse_typescript_file; .tsx/.jsx and plain JS use
//!   the TSX grammar so JSX parses) — module = <package.json name>::<path under
//!   pkg root> (leading `src` and a trailing `index` collapse). Imports build
//!   the same alias map (`import {a as b} from './m'`, `import * as ns`,
//!   default imports, `const x = require('./m')`), with relative sources
//!   resolved to the target file's module via the SAME path math so internal
//!   calls resolve. Calls: bare foo() (alias-first — imported fns are the
//!   dominant call form), this.m() -> the caller's class, Ns.m()/Class.m() via
//!   alias map or same-module class; unknown lower-case receivers are kept as
//!   written (code:extern, still matched by bare-leaf callers_of queries).
//!
//! JAVA (parse_java_file) — module = the file's `package` declaration
//!   (dots -> ::). Imports (`import a.b.C;`, `import static a.b.C.m;`,
//!   wildcards) feed the alias map; classes/interfaces/enums/records nest
//!   (module::Outer::Inner::method); `new Foo()` records a call to the
//!   constructor `Foo::Foo`; bare calls resolve alias-first (static imports)
//!   then to the enclosing class; Upper.m() via alias map or same-package class.
//!
//! PYTHON (parse_python_file) — module = the file's package path (ancestor dirs
//!   carrying `__init__.py`, then the file stem; `__init__.py` collapses to its
//!   package). `class` bodies nest (module::Class::method); `from m import f`
//!   binds the leaf in the alias map so bare `f()` resolves to `m::f`, while a
//!   plain `import a.b` only records the use target (the head package stays an
//!   extern receiver); `self.m()` -> the enclosing class; unknown receivers are
//!   kept as written (code:extern, still matched by bare-leaf callers_of queries).
//!
//! index() is idempotent: it DELETEs prior code:* nodes for the corpus (edges
//! cascade via brain_vault_edges ON DELETE CASCADE), then re-extracts.

use anyhow::Result;
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser};
use uuid::Uuid;

/// Summary of a Cortex indexing run.
#[derive(Debug, Default, Clone)]
pub struct CortexStats {
    pub files_parsed: usize,
    pub symbols: usize,
    pub calls_total: usize,
    pub calls_resolved: usize,
    pub imports: usize,
    pub contains: usize,
    pub inherited_memberships: usize,
}

/// A symbol extracted from a source file.
#[derive(Debug, Clone)]
struct Symbol {
    qualified_name: String,
    node_type: &'static str,
    /// Byte span in the source — used for innermost-enclosing-fn attribution.
    start: usize,
    end: usize,
    /// Index of the parent symbol in the per-file symbol vec (impl->method etc).
    parent: Option<usize>,
}

/// Byte offsets of every line start in a source file (index 0 = start of file,
/// then one past each `\n`). Used to convert a symbol's byte span → 1-based line
/// numbers without re-walking the whole string per symbol.
fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// 1-based line number containing byte offset `byte`, given the file's sorted
/// line-start offsets. Pure + unit-tested.
fn byte_to_line(line_starts: &[usize], byte: usize) -> i32 {
    // The line number is the count of line-starts at or before `byte`.
    line_starts.partition_point(|&s| s <= byte).max(1) as i32
}

/// A call site found inside a function body.
#[derive(Debug, Clone)]
struct CallSite {
    /// The raw path text as written, e.g. `crate::model_runtime::load_model`.
    raw_path: String,
    /// Byte offset of the call (for enclosing-fn attribution).
    at: usize,
}

/// Source language of a parsed file — selects the call-resolution rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    /// TypeScript / TSX / JavaScript (all parsed by the TS/TSX grammars).
    TypeScript,
    Java,
    Python,
}

/// Languages `index()` accepts (also drives the terminal's auto-detect filter).
pub const SUPPORTED_LANGS: &[&str] = &["rust", "typescript", "javascript", "java", "python"];

/// Per-file parse result.
struct FileParse {
    lang: Lang,
    /// Module prefix for this file, e.g. `ff_agent::model_runtime`.
    module: String,
    /// The crate name, e.g. `ff_agent` (TS: package ident; Java: package path).
    crate_name: String,
    symbols: Vec<Symbol>,
    calls: Vec<CallSite>,
    /// `use` targets (fully expanded), for code:import nodes.
    use_targets: Vec<String>,
    /// alias -> fully-qualified module path (e.g. model_runtime -> ff_agent::model_runtime).
    alias_map: HashMap<String, String>,
}

// ─── Public entrypoint ───────────────────────────────────────────────────────

/// File-extension LIKE patterns per language for the content:file query.
fn lang_patterns(lang: &str) -> Result<Vec<String>> {
    let pats: &[&str] = match lang {
        "rust" => &["%.rs"],
        "typescript" => &["%.ts", "%.tsx", "%.mts", "%.cts"],
        "javascript" => &["%.js", "%.jsx", "%.mjs", "%.cjs"],
        "java" => &["%.java"],
        "python" => &["%.py"],
        _ => anyhow::bail!(
            "cortex: --lang must be one of {} (got '{lang}')",
            SUPPORTED_LANGS.join("/")
        ),
    };
    Ok(pats.iter().map(|s| s.to_string()).collect())
}

/// Index a corpus's code files into the Brain faceted graph (single language).
///
/// Re-uses the cached `PgPool` (passed in). Reads only the file-system files that
/// the corpus already scanned as `content:file` nodes; writes only graph rows.
/// Idempotent: wipes all prior code:* nodes for the corpus first. For
/// multi-language repos use [`index_langs`], which wipes ONCE then indexes each
/// language (back-to-back `index` calls would clobber each other's nodes).
pub async fn index(pool: &PgPool, corpus_slug: &str, lang: &str) -> Result<CortexStats> {
    lang_patterns(lang)?; // validate before wiping
    wipe_code_nodes(pool, corpus_slug).await?;
    clear_file_index(pool, corpus_slug).await?; // reset the incremental ledger
    index_one(pool, corpus_slug, lang).await
}

/// Index several languages into one corpus: wipe once, then extract each.
pub async fn index_langs(
    pool: &PgPool,
    corpus_slug: &str,
    langs: &[String],
) -> Result<Vec<(String, CortexStats)>> {
    // Validate every language up front so a bad one doesn't wipe the graph.
    for l in langs {
        lang_patterns(l)?;
    }
    wipe_code_nodes(pool, corpus_slug).await?;
    // A full reindex re-stamps every file's hash from scratch — drop the prior
    // ledger so removed files don't linger as "already indexed" rows. index_one
    // records each file's current hash as it extracts.
    clear_file_index(pool, corpus_slug).await?;
    let mut out = Vec::with_capacity(langs.len());
    for l in langs {
        let stats = index_one(pool, corpus_slug, l).await?;
        out.push((l.clone(), stats));
    }
    Ok(out)
}

/// Summary of an incremental reindex: which files were touched + per-language
/// extraction stats for the changed subset.
#[derive(Debug, Default, Clone)]
pub struct IncrementalReport {
    pub files_changed: usize,
    pub files_unchanged: usize,
    pub files_deleted: usize,
    pub per_lang: Vec<(String, CortexStats)>,
}

/// Classify the corpus's current files against the incremental ledger.
/// Returns `(changed_or_new, unchanged_count, deleted_paths)`:
///   - changed/new: file whose current `content_hash` differs from (or is
///     absent in) the ledger — must be re-extracted.
///   - unchanged: hash matches the ledger — left untouched.
///   - deleted: a ledger path no longer present among current files — its
///     symbols must be removed.
/// Pure (no I/O) so the partition rule is unit-tested directly.
fn partition_changes(
    tracked: &HashMap<String, String>,
    current: &[(String, FileRow)],
) -> (Vec<(String, FileRow)>, usize, Vec<String>) {
    let current_paths: HashSet<&str> = current.iter().map(|(_, fr)| fr.path.as_str()).collect();
    let mut changed: Vec<(String, FileRow)> = Vec::new();
    let mut unchanged = 0usize;
    for (lang, fr) in current {
        if tracked.get(&fr.path).is_some_and(|h| *h == fr.content_hash) {
            unchanged += 1;
        } else {
            changed.push((lang.clone(), fr.clone()));
        }
    }
    let mut deleted: Vec<String> = tracked
        .keys()
        .filter(|p| !current_paths.contains(p.as_str()))
        .cloned()
        .collect();
    deleted.sort(); // deterministic order for callers/tests
    (changed, unchanged, deleted)
}

/// Reindex only the files whose content changed since the last index.
///
/// Compares each `content:file` node's current `content_hash` (refreshed by the
/// corpus scan that runs immediately before this) against the hash Cortex last
/// indexed the file at (`cortex_file_index`). Unchanged files are left exactly
/// as they are — no DB writes. Changed/new files are re-extracted; removed files
/// (gone on disk) have their symbols deleted. Node ids are keyed by stable
/// `code://` path and `add_edge` is idempotent, so cross-file `calls` edges into
/// unchanged callers stay intact and an extern placeholder that gains a real
/// definition simply flips node_type in place.
///
/// Changed files keep their symbol NODES (so incoming `calls` edges from
/// unchanged callers survive the stable-uuid upsert); only each changed file's
/// OUTGOING edges are cleared and re-extracted, and symbols the file no longer
/// defines are GC'd afterward. Removed files have their symbols deleted outright.
///
/// Tradeoff vs a full reindex: `code:extern`/`code:import` nodes that go
/// unreferenced are not garbage-collected here; a periodic full `index_langs`
/// cleans them up. First run on a corpus with no ledger treats every file as
/// changed — equivalent to a full reindex but without the upfront global wipe.
pub async fn index_langs_incremental(
    pool: &PgPool,
    corpus_slug: &str,
    langs: &[String],
) -> Result<IncrementalReport> {
    for l in langs {
        lang_patterns(l)?;
    }
    let corpus_id: Uuid = sqlx::query_scalar("SELECT id FROM brain_corpora WHERE slug = $1")
        .bind(corpus_slug)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no corpus with slug '{corpus_slug}'"))?;

    // What Cortex last indexed: file_path -> indexed_hash.
    let tracked: HashMap<String, String> =
        sqlx::query("SELECT file_path, indexed_hash FROM cortex_file_index WHERE corpus_slug = $1")
            .bind(corpus_slug)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("file_path"),
                    r.get::<String, _>("indexed_hash"),
                )
            })
            .collect();

    // Current files (per language, with the scan's fresh content_hash).
    let mut current: Vec<(String, FileRow)> = Vec::new();
    for l in langs {
        for fr in fetch_file_rows(pool, corpus_slug, l).await? {
            current.push((l.clone(), fr));
        }
    }
    // Deletion signal is the FILESYSTEM, not the content:file node: the corpus
    // scan leaves a stale node (valid_until NULL, old hash) for an in-root file
    // that was removed, so it would otherwise read as "unchanged" and its
    // symbols would never be GC'd. Drop current rows whose file is gone on disk;
    // they then fall into the `deleted` bucket below (tracked − live).
    current.retain(|(_, fr)| Path::new(&fr.path).exists());
    // Partition into changed/new vs unchanged vs deleted (pure; unit-tested).
    let (changed, unchanged_count, deleted) = partition_changes(&tracked, &current);
    let mut report = IncrementalReport {
        files_unchanged: unchanged_count,
        ..IncrementalReport::default()
    };

    // Drop symbols of removed files first (so their fns leave internal_fns).
    for path in &deleted {
        if let Some(fid) = lookup_file_node(pool, corpus_slug, path).await? {
            wipe_file_symbols(pool, fid).await?;
        }
        sqlx::query("DELETE FROM cortex_file_index WHERE corpus_slug = $1 AND file_path = $2")
            .bind(corpus_slug)
            .bind(path)
            .execute(pool)
            .await?;
        report.files_deleted += 1;
    }

    report.files_changed = changed.len();
    if changed.is_empty() {
        return Ok(report);
    }

    // Changed files: capture their OLD symbol ids, clear their OUTGOING edges
    // (calls/contains/imports — extraction re-adds them) but KEEP the nodes so
    // incoming `calls` edges from unchanged callers survive the stable-uuid
    // upsert. GC removed symbols after re-extraction (below).
    let changed_file_ids: Vec<Uuid> = changed.iter().map(|(_, fr)| fr.id).collect();
    let pre_symbol_ids = file_symbol_ids(pool, &changed_file_ids).await?;
    let changed_old_fns = fn_titles_for_ids(pool, &pre_symbol_ids).await?;
    let mut outgoing_src = pre_symbol_ids.clone();
    outgoing_src.extend_from_slice(&changed_file_ids);
    delete_outgoing_edges(pool, &outgoing_src).await?;

    // internal_fns covers the WHOLE corpus so a changed file's call into an
    // unchanged file resolves. Start from every corpus function, drop the
    // changed files' OLD functions (some may have been removed/renamed), then
    // extract_files re-adds the changed files' CURRENT functions in pass 1.
    let mut internal_fns = load_internal_fns(pool, corpus_slug).await?;
    for f in &changed_old_fns {
        internal_fns.remove(f);
    }

    // Re-extract changed files, grouped by language.
    for l in langs {
        let rows: Vec<FileRow> = changed
            .iter()
            .filter(|(lang, _)| lang == l)
            .map(|(_, fr)| fr.clone())
            .collect();
        if rows.is_empty() {
            continue;
        }
        let stats =
            extract_files(pool, corpus_id, corpus_slug, l, &rows, &mut internal_fns).await?;
        report.per_lang.push((l.clone(), stats));
    }

    // GC: symbols that belonged to a changed file before but were not re-created
    // by extraction (renamed/removed). Their nodes were kept above; delete them
    // now (incoming edges cascade — the symbol is genuinely gone).
    let post_set: HashSet<Uuid> = file_symbol_ids(pool, &changed_file_ids)
        .await?
        .into_iter()
        .collect();
    let removed: Vec<Uuid> = pre_symbol_ids
        .into_iter()
        .filter(|id| !post_set.contains(id))
        .collect();
    if !removed.is_empty() {
        delete_nodes_by_id(pool, &removed).await?;
    }
    Ok(report)
}

/// Idempotency: drop all prior code:* nodes for this corpus (edges cascade).
async fn wipe_code_nodes(pool: &PgPool, corpus_slug: &str) -> Result<()> {
    sqlx::query(
        "DELETE FROM brain_vault_nodes
           WHERE project = $1 AND node_type LIKE 'code:%'",
    )
    .bind(corpus_slug)
    .execute(pool)
    .await?;
    Ok(())
}

// ─── Incremental-reindex ledger helpers ──────────────────────────────────────

/// Drop the whole incremental ledger for a corpus (full reindex re-stamps it).
async fn clear_file_index(pool: &PgPool, corpus_slug: &str) -> Result<()> {
    sqlx::query("DELETE FROM cortex_file_index WHERE corpus_slug = $1")
        .bind(corpus_slug)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record (upsert) the hash Cortex indexed a file at.
async fn record_file_hash(
    pool: &PgPool,
    corpus_slug: &str,
    file_path: &str,
    hash: &str,
) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO cortex_file_index (corpus_slug, file_path, indexed_hash, indexed_at)
           VALUES ($1, $2, $3, NOW())
           ON CONFLICT (corpus_slug, file_path)
           DO UPDATE SET indexed_hash = EXCLUDED.indexed_hash, indexed_at = NOW()"#,
    )
    .bind(corpus_slug)
    .bind(file_path)
    .bind(hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Resolve a `content:file` node id by path, even if soft-deleted (valid_until
/// set by the scan when a file disappears), so we can still wipe its symbols.
async fn lookup_file_node(pool: &PgPool, corpus_slug: &str, path: &str) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM brain_vault_nodes
           WHERE project = $1 AND path = $2 AND node_type = 'content:file'
           ORDER BY valid_until NULLS FIRST
           LIMIT 1",
    )
    .bind(corpus_slug)
    .bind(path)
    .fetch_optional(pool)
    .await?)
}

/// Delete all `code:*` symbols owned by one file — the `contains` subtree rooted
/// at the file node (edges cascade), plus the file's own outgoing `imports`
/// edges. Shared `code:import`/`code:extern` nodes are intentionally left (they
/// may be referenced by other files); a full reindex GCs any that go orphaned.
async fn wipe_file_symbols(pool: &PgPool, file_node_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"WITH RECURSIVE descend(id) AS (
               SELECT e.dst_id FROM brain_vault_edges e
                WHERE e.src_id = $1 AND e.edge_type = 'contains'
               UNION
               SELECT e.dst_id FROM brain_vault_edges e
                 JOIN descend d ON e.src_id = d.id
                WHERE e.edge_type = 'contains'
           )
           DELETE FROM brain_vault_nodes
            WHERE id IN (SELECT id FROM descend)
              AND node_type LIKE 'code:%'"#,
    )
    .bind(file_node_id)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM brain_vault_edges WHERE src_id = $1 AND edge_type = 'imports'")
        .bind(file_node_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// All `code:*` symbol node ids owned by the given files — the `contains`
/// subtree rooted at each file node. Used to (a) clear a changed file's old
/// outgoing edges without deleting the nodes and (b) GC symbols the file no
/// longer defines after re-extraction.
async fn file_symbol_ids(pool: &PgPool, file_ids: &[Uuid]) -> Result<Vec<Uuid>> {
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = sqlx::query_scalar(
        r#"WITH RECURSIVE descend(id) AS (
               SELECT e.dst_id FROM brain_vault_edges e
                WHERE e.src_id = ANY($1) AND e.edge_type = 'contains'
               UNION
               SELECT e.dst_id FROM brain_vault_edges e
                 JOIN descend d ON e.src_id = d.id
                WHERE e.edge_type = 'contains'
           )
           SELECT n.id FROM brain_vault_nodes n
             JOIN descend dd ON dd.id = n.id
            WHERE n.node_type LIKE 'code:%'"#,
    )
    .bind(file_ids)
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Of the given node ids, the qualified-names that are `code:function`s.
async fn fn_titles_for_ids(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar(
        "SELECT title FROM brain_vault_nodes WHERE id = ANY($1) AND node_type = 'code:function'",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?)
}

/// Delete the `calls`/`contains`/`imports` edges originating at any of these
/// nodes (a changed file's stale outgoing edges; extraction re-adds the live
/// ones). Incoming edges are left untouched, so unchanged callers keep pointing
/// at the surviving (stable-uuid) symbols.
async fn delete_outgoing_edges(pool: &PgPool, src_ids: &[Uuid]) -> Result<()> {
    if src_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "DELETE FROM brain_vault_edges
           WHERE src_id = ANY($1) AND edge_type IN ('calls', 'contains', 'imports')",
    )
    .bind(src_ids)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete nodes by id (their remaining edges cascade).
async fn delete_nodes_by_id(pool: &PgPool, ids: &[Uuid]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query("DELETE FROM brain_vault_nodes WHERE id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await?;
    Ok(())
}

/// Every internal function qualified-name in the corpus (across all languages),
/// so an incremental reindex of one file resolves calls into unchanged files.
async fn load_internal_fns(pool: &PgPool, corpus_slug: &str) -> Result<HashSet<String>> {
    let titles: Vec<String> = sqlx::query_scalar(
        "SELECT title FROM brain_vault_nodes
           WHERE project = $1 AND node_type = 'code:function'",
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;
    Ok(titles.into_iter().collect())
}

/// A `content:file` node Cortex extracts from, with the corpus scan's hash.
#[derive(Debug, Clone)]
struct FileRow {
    id: Uuid,
    path: String,
    content_hash: String,
}

/// Pull every current `content:file` node for this corpus in this language.
async fn fetch_file_rows(pool: &PgPool, corpus_slug: &str, lang: &str) -> Result<Vec<FileRow>> {
    let patterns = lang_patterns(lang)?;
    let rows = sqlx::query(
        r#"SELECT n.id, n.path, n.content_hash
             FROM brain_vault_nodes n
            WHERE n.project = $1
              AND n.valid_until IS NULL
              AND n.node_type = 'content:file'
              AND n.path LIKE ANY($2)"#,
    )
    .bind(corpus_slug)
    .bind(&patterns)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FileRow {
            id: r.get("id"),
            path: r.get("path"),
            content_hash: r.get("content_hash"),
        })
        .collect())
}

/// Extract one language's symbols/edges for a corpus (no wipe — see callers).
async fn index_one(pool: &PgPool, corpus_slug: &str, lang: &str) -> Result<CortexStats> {
    // Resolve corpus id.
    let corpus_id: Uuid = sqlx::query_scalar("SELECT id FROM brain_corpora WHERE slug = $1")
        .bind(corpus_slug)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no corpus with slug '{corpus_slug}'"))?;

    let file_rows = fetch_file_rows(pool, corpus_slug, lang).await?;
    // Full per-language extraction starts with an empty internal-fn set: the
    // graph was just wiped, so every internal fn comes from these files.
    let mut internal_fns: HashSet<String> = HashSet::new();
    extract_files(
        pool,
        corpus_id,
        corpus_slug,
        lang,
        &file_rows,
        &mut internal_fns,
    )
    .await
}

/// Two-pass extraction over a set of files: write symbol nodes + contains +
/// imports (pass 1, also populating `internal_fns`), then resolve + write
/// `calls` edges (pass 2). `internal_fns` may be pre-seeded (incremental reindex
/// seeds it from the whole-corpus DB so calls into unchanged files resolve).
/// Records each file's current `content_hash` in the incremental ledger.
async fn extract_files(
    pool: &PgPool,
    corpus_id: Uuid,
    corpus_slug: &str,
    lang: &str,
    file_rows: &[FileRow],
    internal_fns: &mut HashSet<String>,
) -> Result<CortexStats> {
    let mut stats = CortexStats::default();

    // First pass: parse every file, write symbol nodes + contains + imports +
    // remember each file's resolved call list. We resolve calls in a second pass
    // once ALL internal symbols are known (so internal vs extern is correct).
    struct Pending {
        file_node_id: Uuid,
        file_path: String,
        parse: FileParse,
        /// symbol qualified_name -> its brain_vault_nodes id (this file's symbols).
        sym_ids: HashMap<String, Uuid>,
    }
    let mut pending: Vec<Pending> = Vec::new();

    for fr in file_rows {
        let file_node_id: Uuid = fr.id;
        let file_path: String = fr.path.clone();
        // Stamp the ledger up front: once we've considered a file at this hash we
        // won't reprocess it next run — even if it has no extractable symbols
        // (.d.ts, unparseable) — which keeps incremental runs from re-churning it.
        record_file_hash(pool, corpus_slug, &file_path, &fr.content_hash).await?;
        if file_path.ends_with(".d.ts") {
            continue; // ambient declaration files: no bodies, all noise
        }
        let source = match std::fs::read_to_string(&file_path) {
            Ok(s) => s,
            Err(_) => continue, // file vanished since scan; skip
        };
        let parse = match lang {
            "rust" => parse_rust_file(&file_path, &source),
            "typescript" | "javascript" => parse_typescript_file(&file_path, &source),
            "java" => parse_java_file(&file_path, &source),
            "python" => parse_python_file(&file_path, &source),
            _ => unreachable!("lang validated by lang_patterns"),
        };
        let parse = match parse {
            Some(p) => p,
            None => continue,
        };
        stats.files_parsed += 1;

        // Line-start offsets for this file, so each symbol's byte span maps to
        // 1-based source lines (persisted on the node for hunk-level review).
        let line_starts = line_start_offsets(&source);

        // Write symbol nodes + contains edges.
        let mut sym_ids: HashMap<String, Uuid> = HashMap::new();
        let mut idx_to_id: HashMap<usize, Uuid> = HashMap::new();
        for (i, sym) in parse.symbols.iter().enumerate() {
            let sym_path = format!("code://{corpus_slug}/{}", sym.qualified_name);
            let start_line = byte_to_line(&line_starts, sym.start);
            // end byte is exclusive; use the last byte of the span for the line.
            let end_line = byte_to_line(&line_starts, sym.end.saturating_sub(1));
            let id = upsert_code_node(
                pool,
                &sym_path,
                &sym.qualified_name,
                sym.node_type,
                corpus_slug,
                Some(start_line),
                Some(end_line.max(start_line)),
            )
            .await?;
            sym_ids.insert(sym.qualified_name.clone(), id);
            idx_to_id.insert(i, id);
            stats.symbols += 1;
            if sym.node_type == "code:function" {
                internal_fns.insert(sym.qualified_name.clone());
            }

            // contains: parent (impl/mod) -> symbol, else file -> symbol.
            let src = match sym.parent.and_then(|p| idx_to_id.get(&p)) {
                Some(parent_id) => *parent_id,
                None => file_node_id,
            };
            if add_edge(pool, src, id, "contains").await? {
                stats.contains += 1;
            }

            // membership/facet inheritance from the file node onto this symbol.
            stats.inherited_memberships +=
                inherit_from_file(pool, corpus_id, file_node_id, id).await?;
        }

        // imports: file -> code:import node (fully-qualified use target).
        for target in &parse.use_targets {
            let imp_path = format!("code://{corpus_slug}/use:{target}");
            let imp_id = upsert_code_node(
                pool,
                &imp_path,
                target,
                "code:import",
                corpus_slug,
                None,
                None,
            )
            .await?;
            if add_edge(pool, file_node_id, imp_id, "imports").await? {
                stats.imports += 1;
            }
        }

        pending.push(Pending {
            file_node_id,
            file_path,
            parse,
            sym_ids,
        });
    }

    // Second pass: resolve calls and write calls edges.
    for p in &pending {
        // Build enclosing-fn lookup for this file: for each call, find the
        // innermost code:function whose [start,end) byte span contains it.
        for call in &p.parse.calls {
            let Some(caller_qn) = innermost_fn(&p.parse.symbols, call.at) else {
                continue;
            };
            let Some(&caller_id) = p.sym_ids.get(&caller_qn) else {
                continue;
            };
            let resolved = resolve_call(&call.raw_path, &caller_qn, &p.parse);
            stats.calls_total += 1;

            // Find the callee node: internal real fn if known, else code:extern.
            let callee_path = format!("code://{corpus_slug}/{resolved}");
            let callee_id = if internal_fns.contains(&resolved) {
                // Internal fn: it has a real code:function node somewhere.
                lookup_code_node(pool, &callee_path).await?
            } else {
                None
            };
            let callee_id = match callee_id {
                Some(id) => {
                    stats.calls_resolved += 1;
                    id
                }
                None => {
                    // External / unresolved: a code:extern placeholder on the same
                    // code:// path, so callers_of still traverses to it.
                    upsert_code_node(
                        pool,
                        &callee_path,
                        &resolved,
                        "code:extern",
                        corpus_slug,
                        None,
                        None,
                    )
                    .await?
                }
            };
            add_edge(pool, caller_id, callee_id, "calls").await?;
        }
        let _ = &p.file_path; // (kept for future per-file diagnostics)
    }

    Ok(stats)
}

// ─── Query side: callers / callees / impact ──────────────────────────────────

#[derive(Debug, Clone)]
pub struct SymbolRef {
    pub id: Uuid,
    pub qualified_name: String,
    pub node_type: String,
}

/// Resolve a user-supplied symbol selector to its node id within a corpus.
/// Accepts a full qualified name (`ff_agent::model_runtime::load_model`) or a
/// bare leaf (`load_model`) — the bare form matches any code:function whose
/// qualified name ends in `::<sel>` (or equals it).
async fn resolve_symbol(pool: &PgPool, corpus_slug: &str, sel: &str) -> Result<Vec<SymbolRef>> {
    // Exact path first.
    let exact_path = format!("code://{corpus_slug}/{sel}");
    let rows = sqlx::query(
        r#"SELECT id, title, node_type FROM brain_vault_nodes
            WHERE project = $1 AND node_type LIKE 'code:%'
              AND (path = $2 OR title = $3 OR title LIKE $4)
            ORDER BY title COLLATE "C""#,
    )
    .bind(corpus_slug)
    .bind(&exact_path)
    .bind(sel)
    .bind(format!("%::{sel}"))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SymbolRef {
            id: r.get("id"),
            qualified_name: r.get("title"),
            node_type: r.get("node_type"),
        })
        .collect())
}

/// Direct callers of a set of symbol node ids: nodes with a `calls` edge whose
/// dst is one of the ids. Querying by id (not by name selector) is exact — no
/// bare-leaf ambiguity — which matters for review, where the ids come straight
/// from a file's `contains` subtree.
async fn callers_of_ids(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<SymbolRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        r#"SELECT DISTINCT n.id, n.title, n.node_type
             FROM brain_vault_edges e
             JOIN brain_vault_nodes n ON n.id = e.src_id
            WHERE e.edge_type = 'calls' AND e.dst_id = ANY($1)
            ORDER BY n.title"#,
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SymbolRef {
            id: r.get("id"),
            qualified_name: r.get("title"),
            node_type: r.get("node_type"),
        })
        .collect())
}

/// Callers of a symbol: nodes with a `calls` edge whose dst is the symbol.
pub async fn callers(pool: &PgPool, corpus_slug: &str, sel: &str) -> Result<Vec<SymbolRef>> {
    let targets = resolve_symbol(pool, corpus_slug, sel).await?;
    let ids: Vec<Uuid> = targets.iter().map(|t| t.id).collect();
    callers_of_ids(pool, &ids).await
}

/// Callees of a symbol: nodes a `calls` edge points to from the symbol.
pub async fn callees(pool: &PgPool, corpus_slug: &str, sel: &str) -> Result<Vec<SymbolRef>> {
    let srcs = resolve_symbol(pool, corpus_slug, sel).await?;
    let ids: Vec<Uuid> = srcs.iter().map(|t| t.id).collect();
    let rows = sqlx::query(
        r#"SELECT DISTINCT n.id, n.title, n.node_type
             FROM brain_vault_edges e
             JOIN brain_vault_nodes n ON n.id = e.dst_id
            WHERE e.edge_type = 'calls' AND e.src_id = ANY($1)
            ORDER BY n.title"#,
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SymbolRef {
            id: r.get("id"),
            qualified_name: r.get("title"),
            node_type: r.get("node_type"),
        })
        .collect())
}

/// Transitive caller closure up to `max_depth` (impact / blast radius).
pub async fn impact(
    pool: &PgPool,
    corpus_slug: &str,
    sel: &str,
    max_depth: usize,
) -> Result<Vec<SymbolRef>> {
    let seed = resolve_symbol(pool, corpus_slug, sel).await?;
    let seed_ids: Vec<Uuid> = seed.iter().map(|s| s.id).collect();
    impact_of_ids(pool, &seed_ids, max_depth).await
}

/// Transitive caller closure of a set of seed node ids (the seeds themselves are
/// excluded from the result). Shared by [`impact`] and the review pass.
async fn impact_of_ids(
    pool: &PgPool,
    seed_ids: &[Uuid],
    max_depth: usize,
) -> Result<Vec<SymbolRef>> {
    let mut frontier: Vec<Uuid> = seed_ids.to_vec();
    let mut seen: HashSet<Uuid> = frontier.iter().copied().collect();
    let mut out: Vec<SymbolRef> = Vec::new();

    for _ in 0..max_depth {
        if frontier.is_empty() {
            break;
        }
        let rows = sqlx::query(
            r#"SELECT DISTINCT n.id, n.title, n.node_type
                 FROM brain_vault_edges e
                 JOIN brain_vault_nodes n ON n.id = e.src_id
                WHERE e.edge_type = 'calls' AND e.dst_id = ANY($1)"#,
        )
        .bind(&frontier)
        .fetch_all(pool)
        .await?;
        let mut next: Vec<Uuid> = Vec::new();
        for r in rows {
            let id: Uuid = r.get("id");
            if seen.insert(id) {
                out.push(SymbolRef {
                    id,
                    qualified_name: r.get("title"),
                    node_type: r.get("node_type"),
                });
                next.push(id);
            }
        }
        frontier = next;
    }
    out.sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name));
    Ok(out)
}

// ─── Test-coverage mapping (ff cortex tests / tests_for) ─────────────────────
//
// CRG exposes `tests_for`; Cortex did not. The question an agent (or a reviewer
// touching a risky symbol) actually asks is "which tests exercise this?" — the
// answer is the transitive caller closure (same reverse-`calls` BFS as `impact`)
// filtered to the callers that ARE tests. Cortex stores no test attribute, so a
// test is detected heuristically from its owning file path and symbol name —
// robust across rust/ts/js/python/java without an index-time schema change.

/// A test function that (transitively) exercises a target symbol.
#[derive(Debug, Clone)]
pub struct TestHit {
    pub id: Uuid,
    pub qualified_name: String,
    /// Absolute path of the owning test file.
    pub file: Option<String>,
    /// Call-graph distance from the target: 1 = the test calls it directly, 2 =
    /// the test calls something that calls it, … Lower = stronger coverage.
    pub depth: usize,
}

/// Does this file path look like a test file? Cross-language heuristic over the
/// (case-insensitive) path: common test directories plus per-language basename
/// conventions. A discovery aid, not a correctness gate — favours recall, so a
/// stray non-test file matching e.g. `*Test.java` is acceptable.
pub fn is_test_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    // Directory conventions (any language).
    if lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.contains("/__tests__/")
        || lower.contains("/spec/")
    {
        return true;
    }
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    // Rust integration / unit files.
    if base.ends_with("_test.rs") || base.ends_with("_tests.rs") {
        return true;
    }
    // JS / TS (.test.* and .spec.*).
    for ext in [
        ".test.ts",
        ".test.tsx",
        ".test.js",
        ".test.jsx",
        ".spec.ts",
        ".spec.tsx",
        ".spec.js",
        ".spec.jsx",
    ] {
        if base.ends_with(ext) {
            return true;
        }
    }
    // Python (pytest / unittest).
    if (base.starts_with("test_") && base.ends_with(".py")) || base.ends_with("_test.py") {
        return true;
    }
    // Java (JUnit: FooTest.java / FooTests.java / FooSpec.java).
    if base.ends_with("test.java") || base.ends_with("tests.java") || base.ends_with("spec.java") {
        return true;
    }
    false
}

/// Does this symbol look like a test? True when its owning file is a test file,
/// or its qualified name carries an in-source test convention: a Rust
/// `#[cfg(test)] mod tests`/`mod test` (`::tests::` / `::test::` in the path) or
/// a `test_`-prefixed leaf (rust/python). Catches inline test modules that live
/// in an otherwise-non-test file.
pub fn is_test_symbol(qualified_name: &str, file: Option<&str>) -> bool {
    if file.map(is_test_file).unwrap_or(false) {
        return true;
    }
    if qualified_name.contains("::tests::") || qualified_name.contains("::test::") {
        return true;
    }
    // Last `::`- or `.`-delimited segment (handles rust paths and dotted langs).
    let leaf = qualified_name
        .rsplit("::")
        .next()
        .unwrap_or(qualified_name)
        .rsplit('.')
        .next()
        .unwrap_or(qualified_name);
    leaf.starts_with("test_")
}

/// Find the test functions that cover `sel`: walk the transitive caller closure
/// (the same reverse-`calls` BFS as [`impact`], restricted to `code:function`
/// nodes) and keep the callers that look like tests ([`is_test_symbol`]). Ranked
/// nearest-first (a direct test caller is stronger coverage than a 5-hop
/// transitive one), then by name. An empty result means no *resolved* test→symbol
/// call edge reaches it within `max_depth` hops — usually a coverage gap, but note
/// it inherits the call graph's resolution limits (e.g. Rust calls made inside a
/// macro — `assert_eq!(foo(), …)` — are not parsed into call edges, so a
/// macro-only-tested Rust fn reads as uncovered; Java/TS/Python method calls
/// resolve directly).
pub async fn tests_for(
    pool: &PgPool,
    corpus_slug: &str,
    sel: &str,
    max_depth: usize,
) -> Result<Vec<TestHit>> {
    let seed = resolve_symbol(pool, corpus_slug, sel).await?;
    if seed.is_empty() {
        anyhow::bail!("no symbol matching '{sel}' in corpus '{corpus_slug}'");
    }
    let max_depth = max_depth.clamp(1, 20);
    let seed_ids: Vec<Uuid> = seed.iter().map(|s| s.id).collect();

    // Reverse BFS over `calls` edges, recording the depth at which each caller is
    // first reached (its shortest call distance from the seed).
    let mut frontier: Vec<Uuid> = seed_ids.clone();
    let mut seen: HashSet<Uuid> = frontier.iter().copied().collect();
    let mut callers: Vec<(Uuid, String, usize)> = Vec::new();
    for depth in 1..=max_depth {
        if frontier.is_empty() {
            break;
        }
        let rows = sqlx::query(
            r#"SELECT DISTINCT n.id, n.title
                 FROM brain_vault_edges e
                 JOIN brain_vault_nodes n ON n.id = e.src_id
                WHERE e.edge_type = 'calls' AND e.dst_id = ANY($1)
                  AND n.node_type = 'code:function'"#,
        )
        .bind(&frontier)
        .fetch_all(pool)
        .await?;
        let mut next: Vec<Uuid> = Vec::new();
        for r in rows {
            let id: Uuid = r.get("id");
            if seen.insert(id) {
                callers.push((id, r.get("title"), depth));
                next.push(id);
            }
        }
        frontier = next;
    }

    // Resolve owning files once, then keep only the test callers.
    let ids: Vec<Uuid> = callers.iter().map(|(id, _, _)| *id).collect();
    let files = owning_files(pool, &ids).await?;
    let mut out: Vec<TestHit> = callers
        .into_iter()
        .filter_map(|(id, qn, depth)| {
            let file = files.get(&id).cloned();
            if is_test_symbol(&qn, file.as_deref()) {
                Some(TestHit {
                    id,
                    qualified_name: qn,
                    file,
                    depth,
                })
            } else {
                None
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then(a.qualified_name.cmp(&b.qualified_name))
    });
    Ok(out)
}

// ─── Symbol discovery (ff cortex find) ───────────────────────────────────────
//
// callers/callees/impact all require the caller to already know a symbol's
// (qualified or leaf) name. `find_symbols` is the missing discovery entrypoint:
// given a name fragment, return matching code symbols ranked by fan-in (most
// depended-on first) with the file:line to jump to — so an agent locates the
// symbol, then drills in with the relationship queries.

/// One hit from [`find_symbols`]: a matched code symbol plus the signals an
/// agent needs — `fan_in` (how many direct callers depend on it, the importance
/// proxy) and `file`/`start_line` (where to jump).
#[derive(Debug, Clone)]
pub struct SymbolHit {
    pub id: Uuid,
    pub qualified_name: String,
    pub node_type: String,
    /// Absolute path of the owning `content:file` node (None for extern/import
    /// placeholders that no file contains).
    pub file: Option<String>,
    /// 1-based start line (None for pre-V124 nodes or non-spanning placeholders).
    pub start_line: Option<i32>,
    pub fan_in: i64,
    /// Cosine similarity (0..=1) when this hit came from `--semantic` ranking;
    /// `None` for substring matches (which rank by `fan_in`, not relevance).
    pub score: Option<f32>,
}

/// Escape SQL `LIKE`/`ILIKE` wildcards (`%`, `_`) and the escape char (`\`) in a
/// user query so a search for `load_model` matches the literal underscore rather
/// than "any single char". Paired with `ESCAPE '\'` in the query below.
pub fn escape_like(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for ch in q.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Map a `--kind` keyword to the set of `code:*` node_types it selects, or
/// `None` if the keyword is unrecognized (the caller errors). Single leaf kinds
/// (`function`, `struct`, …) select that exact `code:<kind>`; `type` is an
/// ergonomic cross-language alias for the type-defining symbols (struct/enum/
/// trait — Rust — plus class/interface — TS/Java/Python) so an agent can ask
/// for "the type called Foo" without knowing which language declared it.
pub fn kind_filter_types(kind: &str) -> Option<Vec<&'static str>> {
    let v = match kind {
        "function" | "fn" => vec!["code:function"],
        "struct" => vec!["code:struct"],
        "enum" => vec!["code:enum"],
        "trait" => vec!["code:trait"],
        "impl" => vec!["code:impl"],
        "mod" | "module" => vec!["code:mod"],
        "class" => vec!["code:class"],
        "interface" => vec!["code:interface"],
        "type" => vec![
            "code:struct",
            "code:enum",
            "code:trait",
            "code:class",
            "code:interface",
        ],
        _ => return None,
    };
    Some(v)
}

/// Resolve an optional `--kind` keyword into the bind value for the `node_type =
/// ANY($k)` filter: `None` (no filter) stays `None`; an unknown keyword is a
/// loud error (rather than silently matching nothing). Owned `String`s so the
/// vector binds directly as a Postgres `text[]`.
fn resolve_kind_filter(kind: Option<&str>) -> Result<Option<Vec<String>>> {
    match kind {
        None => Ok(None),
        Some(k) => match kind_filter_types(k) {
            Some(types) => Ok(Some(types.iter().map(|s| s.to_string()).collect())),
            None => anyhow::bail!(
                "unknown --kind '{k}' (expected one of: function, struct, enum, trait, \
                 impl, mod, class, interface, type)"
            ),
        },
    }
}

/// Find code symbols whose qualified name contains `query` (case-insensitive),
/// ranked by fan-in desc then name, capped at `limit` (clamped to 1..=500).
/// `kind` optionally narrows to a node-type class (see [`kind_filter_types`]).
/// The discovery entrypoint for the relationship queries.
pub async fn find_symbols(
    pool: &PgPool,
    corpus_slug: &str,
    query: &str,
    limit: i64,
    kind: Option<&str>,
) -> Result<Vec<SymbolHit>> {
    let pattern = format!("%{}%", escape_like(query));
    let limit = limit.clamp(1, 500);
    let kind_types = resolve_kind_filter(kind)?;
    let rows = sqlx::query(
        r#"SELECT n.id, n.title, n.node_type, n.start_line,
                  (SELECT count(*) FROM brain_vault_edges e
                    WHERE e.edge_type = 'calls' AND e.dst_id = n.id) AS fan_in
             FROM brain_vault_nodes n
            WHERE n.project = $1
              AND n.node_type LIKE 'code:%'
              AND n.title ILIKE $2 ESCAPE '\'
              AND ($4::text[] IS NULL OR n.node_type = ANY($4))
            ORDER BY fan_in DESC, n.title COLLATE "C"
            LIMIT $3"#,
    )
    .bind(corpus_slug)
    .bind(&pattern)
    .bind(limit)
    .bind(kind_types.as_deref())
    .fetch_all(pool)
    .await?;

    let mut hits: Vec<SymbolHit> = rows
        .into_iter()
        .map(|r| SymbolHit {
            id: r.get("id"),
            qualified_name: r.get("title"),
            node_type: r.get("node_type"),
            file: None,
            start_line: r.get("start_line"),
            fan_in: r.get("fan_in"),
            score: None,
        })
        .collect();

    resolve_hit_files(pool, &mut hits).await?;
    Ok(hits)
}

/// Resolve each hit's owning file by walking `contains` edges UP to the ancestor
/// `content:file` node (file -> impl/mod -> symbol can nest, so a recursive walk,
/// not a single hop). Fills `SymbolHit::file` in place. Shared by the substring
/// and semantic `find_symbols*` paths.
async fn resolve_hit_files(pool: &PgPool, hits: &mut [SymbolHit]) -> Result<()> {
    let ids: Vec<Uuid> = hits.iter().map(|h| h.id).collect();
    let by_leaf = owning_files(pool, &ids).await?;
    for h in hits.iter_mut() {
        h.file = by_leaf.get(&h.id).cloned();
    }
    Ok(())
}

/// For each of `ids`, the absolute path of the `content:file` node that
/// (transitively, via `contains` edges) owns it — walking UP from the symbol
/// through any nesting (`file → impl/mod → symbol`). Symbols with no owning file
/// (extern/import placeholders) are simply absent from the map. Shared by
/// [`resolve_hit_files`] and [`tests_for`].
async fn owning_files(pool: &PgPool, ids: &[Uuid]) -> Result<HashMap<Uuid, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let file_rows = sqlx::query(
        r#"WITH RECURSIVE up AS (
                SELECT e.src_id AS anc, e.dst_id AS leaf
                  FROM brain_vault_edges e
                 WHERE e.edge_type = 'contains' AND e.dst_id = ANY($1)
                UNION
                SELECT e.src_id, up.leaf
                  FROM brain_vault_edges e
                  JOIN up ON e.dst_id = up.anc
                 WHERE e.edge_type = 'contains'
            )
            SELECT up.leaf AS leaf, n.path AS path
              FROM up JOIN brain_vault_nodes n ON n.id = up.anc
             WHERE n.node_type = 'content:file'"#,
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    let mut by_leaf: HashMap<Uuid, String> = HashMap::new();
    for r in file_rows {
        by_leaf.insert(r.get("leaf"), r.get("path"));
    }
    Ok(by_leaf)
}

/// Map a pgvector `<->` distance to a bounded 0..=1 similarity (higher = closer),
/// matching `vector_search`'s scoring so the printed number is comparable across
/// queries. Distance 0 → 1.0; grows monotonically less as distance increases.
pub fn similarity_from_distance(distance: f64) -> f32 {
    (1.0f32 / (1.0f32 + distance as f32)).min(1.0f32)
}

/// Semantic variant of [`find_symbols`]: embed `query` via the fleet's bge-m3
/// endpoint and rank `code:*` symbols in the corpus by embedding distance
/// (pgvector `<->`), instead of by name substring. Use when the caller knows the
/// *intent* ("where do we publish heartbeats") but not the exact name — substring
/// search misses those. Returns the same [`SymbolHit`] shape (now with `score`
/// set) so a hit drills into callers/callees/impact identically.
///
/// Errors — rather than silently degrading — when no fleet embedding endpoint is
/// live (the query would otherwise embed to hash-stub noise and rank garbage) or
/// when the corpus has no embedded nodes yet (run `ff cortex embed` first).
pub async fn find_symbols_semantic(
    pool: &PgPool,
    corpus_slug: &str,
    query: &str,
    limit: i64,
    kind: Option<&str>,
) -> Result<Vec<SymbolHit>> {
    let limit = limit.clamp(1, 500);
    let kind_types = resolve_kind_filter(kind)?;
    let client = crate::embeddings::fleet_embedding_client(pool)
        .await
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no healthy fleet embedding endpoint for --semantic search — load one with \
                 `ff model load <bge-m3-lib-id>` (needs preferred_workloads=embedding), or omit \
                 --semantic for substring search"
            )
        })?;
    let qvec = client
        .embed(query)
        .await
        .map_err(|e| anyhow::anyhow!("embed query: {e}"))?;
    let qlit = crate::vector_search::embedding_to_pgvector(&qvec);

    // `<->` is pgvector's distance operator (L2 for the default opclass); smaller
    // = closer. We surface a bounded 0..=1 similarity via 1/(1+distance), matching
    // how vector_search scores, so the printed number is comparable across queries.
    let rows = sqlx::query(
        r#"SELECT n.id, n.title, n.node_type, n.start_line,
                  (SELECT count(*) FROM brain_vault_edges e
                    WHERE e.edge_type = 'calls' AND e.dst_id = n.id) AS fan_in,
                  (n.embedding <-> $2::vector) AS distance
             FROM brain_vault_nodes n
            WHERE n.project = $1
              AND n.node_type LIKE 'code:%'
              AND n.embedding IS NOT NULL
              AND ($4::text[] IS NULL OR n.node_type = ANY($4))
            ORDER BY n.embedding <-> $2::vector
            LIMIT $3"#,
    )
    .bind(corpus_slug)
    .bind(&qlit)
    .bind(limit)
    .bind(kind_types.as_deref())
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        // Distinguish "nothing similar" from "nothing embedded": the latter is an
        // operator action (run `ff cortex embed`), so say so instead of an empty list.
        let embedded: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM brain_vault_nodes
              WHERE project = $1 AND node_type LIKE 'code:%' AND embedding IS NOT NULL",
        )
        .bind(corpus_slug)
        .fetch_one(pool)
        .await?;
        if embedded == 0 {
            anyhow::bail!(
                "corpus '{corpus_slug}' has no embedded code symbols — run `ff cortex embed` \
                 first, then retry --semantic (or use substring search without --semantic)"
            );
        }
    }

    let mut hits: Vec<SymbolHit> = rows
        .into_iter()
        .map(|r| {
            // pgvector returns FLOAT8 for the distance expression; read as f64.
            let distance: f64 = r.get("distance");
            SymbolHit {
                id: r.get("id"),
                qualified_name: r.get("title"),
                node_type: r.get("node_type"),
                file: None,
                start_line: r.get("start_line"),
                fan_in: r.get("fan_in"),
                score: Some(similarity_from_distance(distance)),
            }
        })
        .collect();

    resolve_hit_files(pool, &mut hits).await?;
    Ok(hits)
}

// ─── Change-aware review (detect_changes vs git diff) ────────────────────────
//
// Given the set of changed files (the terminal layer computes them from
// `git diff`), produce a risk-scored review map: for each changed file, the
// symbols it defines and — for each callable symbol — how many places call it
// (fan-in) and the full transitive blast radius. The point is to tell a
// reviewer (human or agent) WHERE to look first: a one-line tweak to a function
// 40 callers depend on is far riskier than a new private helper nobody calls
// yet. Mirrors CRG's detect_changes + get_review_context, but native to Cortex
// and driven by the graph this loop already keeps fresh.

/// Risk band for a changed symbol or file. Serialized lowercase for JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskTier {
    High,
    Medium,
    Low,
}

impl RiskTier {
    pub fn label(self) -> &'static str {
        match self {
            RiskTier::High => "HIGH",
            RiskTier::Medium => "MED",
            RiskTier::Low => "LOW",
        }
    }
    /// Rank for sorting (High first).
    fn rank(self) -> u8 {
        match self {
            RiskTier::High => 2,
            RiskTier::Medium => 1,
            RiskTier::Low => 0,
        }
    }
}

/// Classify a changed symbol's risk from its blast metrics. Pure + unit-tested.
///   - `blast` is the transitive caller-closure size (how far a break ripples).
///   - `external` is the count of DIRECT callers defined outside the changed
///     file (cross-file fan-in — the part of the change that's a de-facto API).
/// A change with wide reach OR many external dependents is high-risk; a change
/// nothing (or only same-file code) calls is low-risk.
pub fn risk_tier(blast: usize, external: usize) -> RiskTier {
    if blast >= 10 || external >= 5 {
        RiskTier::High
    } else if blast >= 3 || external >= 1 {
        RiskTier::Medium
    } else {
        RiskTier::Low
    }
}

/// One changed symbol with its blast-radius metrics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChangedSymbol {
    pub qualified_name: String,
    pub node_type: String,
    /// Direct callers (one `calls` edge away).
    pub direct_callers: usize,
    /// Direct callers defined OUTSIDE this file (cross-file fan-in).
    pub external_callers: usize,
    /// Transitive caller closure size up to the review depth.
    pub blast_radius: usize,
    pub risk: RiskTier,
    /// A few example impacted callers (qualified names), for the report.
    pub top_callers: Vec<String>,
}

/// Review summary for one changed file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChangedFile {
    /// Absolute path (as stored on the `content:file` node).
    pub path: String,
    pub symbols: Vec<ChangedSymbol>,
    /// Max risk across the file's symbols (Low if it defines none).
    pub risk: RiskTier,
    /// Union of every symbol's transitive caller closure (deduped) — the file's
    /// true blast radius (the same caller reached via two symbols counts once).
    pub blast_radius: usize,
}

/// The full change-aware review report.
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ReviewReport {
    /// Indexed changed files, sorted risk-desc then blast-desc.
    pub files: Vec<ChangedFile>,
    /// Changed source files NOT present in the graph (new files, or files the
    /// corpus hasn't re-scanned yet — reindex to cover them).
    pub unindexed: Vec<String>,
    /// Union blast radius across every changed file (deduped node ids).
    pub total_blast: usize,
    /// True when at least one file was narrowed to the symbols whose bodies
    /// overlap the git-diff line ranges (hunk-level), vs listing every symbol the
    /// file defines (file-level). False if no line ranges were supplied or no
    /// indexed file had usable symbol spans.
    pub hunk_level: bool,
}

/// Look up a `content:file` node id by absolute path within a corpus.
async fn file_node_id(pool: &PgPool, corpus_slug: &str, abs_path: &str) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        r#"SELECT id FROM brain_vault_nodes
            WHERE project = $1 AND node_type = 'content:file' AND path = $2
              AND valid_until IS NULL"#,
    )
    .bind(corpus_slug)
    .bind(abs_path)
    .fetch_optional(pool)
    .await?)
}

/// All `code:*` symbols a file defines — the transitive `contains` subtree from
/// the file node (file -> impl/mod -> method nests one or two levels).
/// A symbol from `symbols_in_file`, carrying its persisted 1-based line span
/// (V124) so `review` can do hunk-level filtering. Lines are `None` for nodes
/// indexed before V124 (or re-pointed import/extern placeholders), in which case
/// review degrades gracefully to file-level (includes the symbol).
struct FileSymbol {
    sref: SymbolRef,
    start_line: Option<i32>,
    end_line: Option<i32>,
}

async fn symbols_in_file(pool: &PgPool, file_id: Uuid) -> Result<Vec<FileSymbol>> {
    let rows = sqlx::query(
        r#"WITH RECURSIVE sub AS (
               SELECT dst_id AS id
                 FROM brain_vault_edges
                WHERE src_id = $1 AND edge_type = 'contains'
               UNION
               SELECT e.dst_id
                 FROM brain_vault_edges e
                 JOIN sub ON e.src_id = sub.id
                WHERE e.edge_type = 'contains'
           )
           SELECT n.id, n.title, n.node_type, n.start_line, n.end_line
             FROM brain_vault_nodes n
             JOIN sub ON n.id = sub.id
            WHERE n.node_type LIKE 'code:%'
            ORDER BY n.title COLLATE "C""#,
    )
    .bind(file_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FileSymbol {
            sref: SymbolRef {
                id: r.get("id"),
                qualified_name: r.get("title"),
                node_type: r.get("node_type"),
            },
            start_line: r.get("start_line"),
            end_line: r.get("end_line"),
        })
        .collect())
}

/// Does a symbol's persisted line span overlap any of the changed `hunks`
/// (1-based inclusive `(start, end)` ranges from the git diff)? A symbol with no
/// recorded span (`None`) is treated as touched — fail-open so review never
/// hides a change just because the node predates V124. Pure + unit-tested.
fn symbol_touched_by_hunks(
    start_line: Option<i32>,
    end_line: Option<i32>,
    hunks: &[(u32, u32)],
) -> bool {
    let (Some(s), Some(e)) = (start_line, end_line) else {
        return true; // unknown span ⇒ can't exclude it
    };
    let (s, e) = (s.max(0) as u32, e.max(0) as u32);
    hunks.iter().any(|&(hs, he)| s <= he && hs <= e)
}

/// Map a file extension to a Cortex language name (`None` = ignored). Pure;
/// shared by every frontend that needs to decide whether a changed file is a
/// Cortex source file (the CLI `ff cortex review` and the `cortex_review` MCP
/// tool both filter with this + [`SUPPORTED_LANGS`]). `py` resolves to
/// `python` even though it is not yet in `SUPPORTED_LANGS`, so language
/// detection can count it while review still skips it.
pub fn ext_lang(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "ts" | "tsx" | "mts" | "cts" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "java" => Some("java"),
        "py" => Some("python"),
        _ => None,
    }
}

/// Parse a unified `git diff` (use `--unified=0` for tight hunks) into the
/// new-file line ranges it touched, keyed by repo-relative path. Reads `+++ b/<p>`
/// for the path and the `+c,d` side of each `@@ -a,b +c,d @@` header. A pure-text
/// function so it is unit-testable without a repo and reusable by any frontend
/// that has a diff (the CLI shells `git diff`, the MCP `cortex_review` tool shells
/// it in the daemon). Pure additions (`+c,d`), modifications, and deletions
/// (`+c,0` → records line `c` so a deleted body still flags its enclosing symbol)
/// are all covered. A `+++ /dev/null` target (whole-file deletion) yields no entry.
pub fn parse_diff_line_ranges(diff: &str) -> HashMap<String, Vec<(u32, u32)>> {
    let mut map: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    let mut cur: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            // "+++ b/path" (or "+++ /dev/null" for a deletion).
            cur = rest
                .strip_prefix("b/")
                .filter(|p| *p != "/dev/null")
                .map(|p| p.to_string());
        } else if let Some(h) = line.strip_prefix("@@") {
            // "@@ -a,b +c,d @@ ...": take the +c,d span.
            if let (Some(path), Some((start, count))) = (cur.as_ref(), parse_hunk_new_span(h)) {
                // count==0 (pure deletion) still touches the line at `start`.
                let len = count.max(1);
                let end = start.saturating_add(len - 1);
                map.entry(path.clone()).or_default().push((start, end));
            }
        }
    }
    map
}

/// Extract the `(start, count)` of the `+c,d` side of a hunk header body (the
/// text after the leading `@@`). `+c` alone means count 1. Returns `None` if no
/// `+` field is present. Pure + unit-tested.
pub fn parse_hunk_new_span(header_body: &str) -> Option<(u32, u32)> {
    let plus = header_body
        .split_whitespace()
        .find(|t| t.starts_with('+'))?;
    let spec = plus.trim_start_matches('+');
    let mut parts = spec.split(',');
    let start: u32 = parts.next()?.parse().ok()?;
    let count: u32 = match parts.next() {
        Some(c) => c.parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

/// Build a change-aware, risk-scored review report for a set of changed files.
///
/// `changed_abs_paths` are absolute filesystem paths (the terminal layer derives
/// them from `git diff` and maps repo-relative → absolute). `depth` bounds the
/// transitive blast-radius walk. Files not in the graph land in `unindexed`.
///
/// `changed_lines`, when supplied, maps an absolute path → the 1-based inclusive
/// line ranges the diff actually touched in that file. For files present in the
/// map, review narrows to the symbols whose bodies overlap those ranges
/// (HUNK-level) instead of every symbol the file defines (file-level). A file
/// absent from the map — a brand-new file, or one whose nodes predate V124 line
/// spans — falls back to file-level, so the feature only ever sharpens the
/// report, never hides a change.
pub async fn review(
    pool: &PgPool,
    corpus_slug: &str,
    changed_abs_paths: &[String],
    depth: usize,
    changed_lines: Option<&HashMap<String, Vec<(u32, u32)>>>,
) -> Result<ReviewReport> {
    let mut report = ReviewReport::default();
    let mut global_blast: HashSet<Uuid> = HashSet::new();

    for path in changed_abs_paths {
        let Some(fid) = file_node_id(pool, corpus_slug, path).await? else {
            report.unindexed.push(path.clone());
            continue;
        };
        let all_syms = symbols_in_file(pool, fid).await?;
        // Hunk-level narrowing: if the diff gave line ranges for THIS file, keep
        // only the symbols whose recorded span overlaps a changed range. Symbols
        // with no recorded span are kept (fail-open). No ranges ⇒ keep all.
        let hunks = changed_lines.and_then(|m| m.get(path));
        let syms: Vec<&FileSymbol> = match hunks {
            Some(ranges) => {
                let kept: Vec<&FileSymbol> = all_syms
                    .iter()
                    .filter(|s| symbol_touched_by_hunks(s.start_line, s.end_line, ranges))
                    .collect();
                // Mark hunk-level only when narrowing actually had spans to act on
                // (some kept symbol carried a real span), so the report flag means
                // "this is line-precise" rather than merely "ranges were passed".
                if kept.iter().any(|s| s.start_line.is_some()) {
                    report.hunk_level = true;
                }
                kept
            }
            None => all_syms.iter().collect(),
        };
        // The file's own symbol names — used to split internal vs external fan-in.
        // Use the FULL symbol set (not the hunk subset): a same-file caller of a
        // changed fn is still internal even if its own body wasn't touched.
        let own_names: HashSet<&str> = all_syms
            .iter()
            .map(|s| s.sref.qualified_name.as_str())
            .collect();

        let mut file_blast: HashSet<Uuid> = HashSet::new();
        let mut changed_syms: Vec<ChangedSymbol> = Vec::new();
        for fs in &syms {
            let s = &fs.sref;
            // Only callable symbols accrue `calls`-edge fan-in; structs/impls
            // are listed with zero metrics so the diff is fully accounted for.
            let (direct, blast) = if s.node_type == "code:function" {
                let direct = callers_of_ids(pool, &[s.id]).await?;
                let blast = impact_of_ids(pool, &[s.id], depth).await?;
                (direct, blast)
            } else {
                (Vec::new(), Vec::new())
            };
            let external = direct
                .iter()
                .filter(|c| !own_names.contains(c.qualified_name.as_str()))
                .count();
            for b in &blast {
                file_blast.insert(b.id);
                global_blast.insert(b.id);
            }
            let mut top_callers: Vec<String> = direct
                .iter()
                .take(5)
                .map(|c| c.qualified_name.clone())
                .collect();
            top_callers.sort();
            changed_syms.push(ChangedSymbol {
                qualified_name: s.qualified_name.clone(),
                node_type: s.node_type.clone(),
                direct_callers: direct.len(),
                external_callers: external,
                blast_radius: blast.len(),
                risk: risk_tier(blast.len(), external),
                top_callers,
            });
        }

        // File risk = the worst symbol it touches (Low if it defines none).
        let file_risk = changed_syms
            .iter()
            .map(|s| s.risk)
            .max_by_key(|r| r.rank())
            .unwrap_or(RiskTier::Low);
        // Sort symbols within the file risk-desc then by blast.
        changed_syms.sort_by(|a, b| {
            b.risk
                .rank()
                .cmp(&a.risk.rank())
                .then(b.blast_radius.cmp(&a.blast_radius))
                .then(a.qualified_name.cmp(&b.qualified_name))
        });
        report.files.push(ChangedFile {
            path: path.clone(),
            symbols: changed_syms,
            risk: file_risk,
            blast_radius: file_blast.len(),
        });
    }

    // Most-actionable first: highest risk, then widest blast.
    report.files.sort_by(|a, b| {
        b.risk
            .rank()
            .cmp(&a.risk.rank())
            .then(b.blast_radius.cmp(&a.blast_radius))
            .then(a.path.cmp(&b.path))
    });
    report.unindexed.sort();
    report.total_blast = global_blast.len();
    Ok(report)
}

// ─── DB helpers ──────────────────────────────────────────────────────────────

async fn upsert_code_node(
    pool: &PgPool,
    path: &str,
    title: &str,
    node_type: &str,
    project: &str,
    start_line: Option<i32>,
    end_line: Option<i32>,
) -> Result<Uuid> {
    // content_hash is NOT NULL; use the path (synthetic + unique) as a stable hash.
    // start_line/end_line are 1-based source spans (V124) — set for real symbol
    // nodes (so `review` can do hunk-level filtering), NULL for import/extern
    // placeholders. On conflict we refresh them so an incremental reindex (which
    // KEEPS the stable node and re-upserts it) tracks the symbol as it moves.
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO brain_vault_nodes
               (path, title, node_type, project, content_hash, start_line, end_line)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           ON CONFLICT (path) DO UPDATE
             SET title = EXCLUDED.title, node_type = EXCLUDED.node_type,
                 project = EXCLUDED.project, valid_until = NULL, updated_at = NOW(),
                 start_line = EXCLUDED.start_line, end_line = EXCLUDED.end_line
           RETURNING id"#,
    )
    .bind(path)
    .bind(title)
    .bind(node_type)
    .bind(project)
    .bind(path)
    .bind(start_line)
    .bind(end_line)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn lookup_code_node(pool: &PgPool, path: &str) -> Result<Option<Uuid>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM brain_vault_nodes WHERE path = $1")
            .bind(path)
            .fetch_optional(pool)
            .await?,
    )
}

/// Returns true if a new edge row was inserted (false if it already existed).
async fn add_edge(pool: &PgPool, src: Uuid, dst: Uuid, edge_type: &str) -> Result<bool> {
    if src == dst && edge_type == "calls" {
        // skip trivial self-loops produced by recursion noise? keep recursion
        // edges — they are real. Only the parse-error false-self case is avoided
        // upstream via ERROR-node descent, so allow self here.
    }
    let r = sqlx::query(
        r#"INSERT INTO brain_vault_edges (src_id, dst_id, edge_type, provenance)
           VALUES ($1, $2, $3, 'cortex')
           ON CONFLICT (src_id, dst_id, edge_type) DO NOTHING"#,
    )
    .bind(src)
    .bind(dst)
    .bind(edge_type)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Copy the file node's memberships + facets onto the symbol node, so faceted
/// SET-INTERSECTION queries (--product X --modality code) cover symbols. Returns
/// the number of membership rows inherited.
async fn inherit_from_file(
    pool: &PgPool,
    corpus_id: Uuid,
    file_node_id: Uuid,
    symbol_id: Uuid,
) -> Result<usize> {
    let m = sqlx::query(
        r#"INSERT INTO brain_memberships
             (corpus_id, member_id, member_kind, entity_id, relation, provenance)
           SELECT $1, $2, 'content', m.entity_id, m.relation, 'cortex'
             FROM brain_memberships m
            WHERE m.member_id = $3 AND m.member_kind = 'content'
           ON CONFLICT (member_id, entity_id, relation) DO NOTHING"#,
    )
    .bind(corpus_id)
    .bind(symbol_id)
    .bind(file_node_id)
    .execute(pool)
    .await?;

    sqlx::query(
        r#"INSERT INTO brain_node_facets
             (corpus_id, node_id, node_kind, facet_id, provenance)
           SELECT $1, $2, 'content', nf.facet_id, 'cortex'
             FROM brain_node_facets nf
            WHERE nf.node_id = $3
           ON CONFLICT (node_id, facet_id) DO NOTHING"#,
    )
    .bind(corpus_id)
    .bind(symbol_id)
    .bind(file_node_id)
    .execute(pool)
    .await?;

    Ok(m.rows_affected() as usize)
}

// ─── Parsing (tree-sitter-rust) ──────────────────────────────────────────────

/// Find the innermost enclosing code:function for a byte offset.
fn innermost_fn(symbols: &[Symbol], at: usize) -> Option<String> {
    symbols
        .iter()
        .filter(|s| s.node_type == "code:function" && s.start <= at && at < s.end)
        .min_by_key(|s| s.end - s.start)
        .map(|s| s.qualified_name.clone())
}

/// Parse a Rust file into its module prefix, symbols, calls, use targets, and
/// alias map. Returns None if the language grammar fails to load.
fn parse_rust_file(file_path: &str, source: &str) -> Option<FileParse> {
    let (crate_name, module) = module_for_file(file_path);

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let bytes = source.as_bytes();

    let mut fp = FileParse {
        lang: Lang::Rust,
        module: module.clone(),
        crate_name: crate_name.clone(),
        symbols: Vec::new(),
        calls: Vec::new(),
        use_targets: Vec::new(),
        alias_map: HashMap::new(),
    };

    // Walk the tree, tracking the current module path (mod blocks) and the
    // current parent symbol index (impl blocks). `module`/`crate_name` are still
    // owned locals here (fp holds its own clones).
    let module_local = module;
    let crate_local = crate_name;
    walk(&root, bytes, &module_local, &crate_local, None, &mut fp);

    Some(fp)
}

/// Recursive descent. `mod_path` is the qualified module prefix at this point;
/// `parent` is the index of the enclosing impl/mod symbol (for contains edges).
fn walk(
    node: &Node,
    bytes: &[u8],
    mod_path: &str,
    crate_name: &str,
    parent: Option<usize>,
    fp: &mut FileParse,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "use_declaration" => {
                collect_use(&child, bytes, crate_name, fp);
            }
            "function_item" => {
                if let Some(name) = child_field_text(&child, "name", bytes) {
                    let qn = join(mod_path, &name);
                    let idx = fp.symbols.len();
                    fp.symbols.push(Symbol {
                        qualified_name: qn.clone(),
                        node_type: "code:function",
                        start: child.start_byte(),
                        end: child.end_byte(),
                        parent,
                    });
                    // descend into the body to collect calls (attributed by span).
                    collect_calls(&child, bytes, fp);
                    // nested items inside a fn keep the same module path.
                    walk(&child, bytes, mod_path, crate_name, Some(idx), fp);
                }
            }
            "struct_item" | "enum_item" | "trait_item" => {
                let nt = match child.kind() {
                    "struct_item" => "code:struct",
                    "enum_item" => "code:enum",
                    _ => "code:trait",
                };
                if let Some(name) = child_field_text(&child, "name", bytes) {
                    let qn = join(mod_path, &name);
                    let idx = fp.symbols.len();
                    fp.symbols.push(Symbol {
                        qualified_name: qn,
                        node_type: nt,
                        start: child.start_byte(),
                        end: child.end_byte(),
                        parent,
                    });
                    walk(&child, bytes, mod_path, crate_name, Some(idx), fp);
                }
            }
            "impl_item" => {
                // impl <Type> { ... } — type name becomes the symbol; methods
                // hang off it via contains; methods' qualified name uses the type.
                let ty =
                    child_field_text(&child, "type", bytes).unwrap_or_else(|| "impl".to_string());
                let qn = join(mod_path, &ty);
                let idx = fp.symbols.len();
                fp.symbols.push(Symbol {
                    qualified_name: qn.clone(),
                    node_type: "code:impl",
                    start: child.start_byte(),
                    end: child.end_byte(),
                    parent,
                });
                // Methods inside the impl: module path stays the file module
                // (so `bare foo()` inside a method resolves to module::foo, which
                // matches free-fn ground truth), parent is the impl for contains.
                walk(&child, bytes, mod_path, crate_name, Some(idx), fp);
            }
            "mod_item" => {
                if let Some(name) = child_field_text(&child, "name", bytes) {
                    let sub_mod = join(mod_path, &name);
                    let qn = sub_mod.clone();
                    let idx = fp.symbols.len();
                    fp.symbols.push(Symbol {
                        qualified_name: qn,
                        node_type: "code:mod",
                        start: child.start_byte(),
                        end: child.end_byte(),
                        parent,
                    });
                    walk(&child, bytes, &sub_mod, crate_name, Some(idx), fp);
                }
            }
            // ERROR-node descent: keep finding functions after a parse error so we
            // don't lose calls / produce false self-edges.
            "ERROR" => {
                walk(&child, bytes, mod_path, crate_name, parent, fp);
            }
            _ => {
                // Recurse generically (declaration lists, attribute items, etc).
                walk(&child, bytes, mod_path, crate_name, parent, fp);
            }
        }
    }
}

/// Collect call sites in a function body (call_expression with a path/identifier
/// function). We record the raw path text + byte offset for later attribution.
fn collect_calls(node: &Node, bytes: &[u8], fp: &mut FileParse) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "call_expression" {
            if let Some(func) = child.child_by_field_name("function") {
                if let Some(raw) = call_target_path(&func, bytes) {
                    fp.calls.push(CallSite {
                        raw_path: raw,
                        at: child.start_byte(),
                    });
                }
            }
        }
        // Recurse — calls can be nested arbitrarily, but NOT into nested
        // function_items (those are separate symbols handled by walk()).
        if child.kind() != "function_item" {
            collect_calls(&child, bytes, fp);
        }
    }
}

/// Extract the callable path text from a call's `function` node.
/// Handles `identifier` (bare) and `scoped_identifier` (a::b::c). Method calls
/// (`field_expression`) are skipped — they need type inference, out of scope.
fn call_target_path(func: &Node, bytes: &[u8]) -> Option<String> {
    match func.kind() {
        "identifier" => node_text(func, bytes),
        "scoped_identifier" => node_text(func, bytes),
        // generic_function: foo::<T>() — strip the turbofish.
        "generic_function" => func
            .child_by_field_name("function")
            .and_then(|f| call_target_path(&f, bytes)),
        _ => None,
    }
}

/// Collect a `use` declaration into use_targets + alias_map. Handles
/// `a::b::c`, `a::b as c`, `a::{b, c}`, and `a::{self, b}`.
fn collect_use(node: &Node, bytes: &[u8], crate_name: &str, fp: &mut FileParse) {
    // The argument child holds the tree (scoped_identifier / use_list / use_as_clause).
    if let Some(arg) = node.child_by_field_name("argument") {
        expand_use(&arg, bytes, "", crate_name, fp);
    } else {
        // Fallback: some grammars expose children directly.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "scoped_identifier" | "use_list" | "use_as_clause" | "identifier"
                | "scoped_use_list" | "use_wildcard" => {
                    expand_use(&child, bytes, "", crate_name, fp);
                }
                _ => {}
            }
        }
    }
}

/// Recursively expand a use-tree node under prefix `pfx` (already normalized).
fn expand_use(node: &Node, bytes: &[u8], pfx: &str, crate_name: &str, fp: &mut FileParse) {
    match node.kind() {
        "identifier" => {
            if let Some(name) = node_text(node, bytes) {
                let full = norm_crate(&join(pfx, &name), crate_name);
                register_use(&full, &name, fp);
            }
        }
        "scoped_identifier" => {
            // path::name — gather full text, register leaf as alias.
            if let Some(full_raw) = node_text(node, bytes) {
                let full = norm_crate(&prefixed(pfx, &full_raw), crate_name);
                let leaf = full.rsplit("::").next().unwrap_or(&full).to_string();
                register_use(&full, &leaf, fp);
            }
        }
        "use_as_clause" => {
            // path as alias
            let path = node
                .child_by_field_name("path")
                .and_then(|p| node_text(&p, bytes));
            let alias = node
                .child_by_field_name("alias")
                .and_then(|a| node_text(&a, bytes));
            if let (Some(path), Some(alias)) = (path, alias) {
                let full = norm_crate(&prefixed(pfx, &path), crate_name);
                fp.use_targets.push(full.clone());
                fp.alias_map.insert(alias, full);
            }
        }
        "scoped_use_list" => {
            // path::{ ... } — `path` field is the prefix, then a use_list.
            let new_pfx = node
                .child_by_field_name("path")
                .and_then(|p| node_text(&p, bytes))
                .map(|p| prefixed(pfx, &p))
                .unwrap_or_else(|| pfx.to_string());
            let new_pfx = norm_crate(&new_pfx, crate_name);
            if let Some(list) = node.child_by_field_name("list") {
                expand_use(&list, bytes, &new_pfx, crate_name, fp);
            } else {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "use_list" {
                        expand_use(&child, bytes, &new_pfx, crate_name, fp);
                    }
                }
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "," | "{" | "}" => {}
                    "self" => {
                        // `use a::b::{self}` binds the parent module `a::b`.
                        if !pfx.is_empty() {
                            let leaf = pfx.rsplit("::").next().unwrap_or(pfx).to_string();
                            register_use(pfx, &leaf, fp);
                        }
                    }
                    _ => expand_use(&child, bytes, pfx, crate_name, fp),
                }
            }
        }
        "self" => {
            if !pfx.is_empty() {
                let leaf = pfx.rsplit("::").next().unwrap_or(pfx).to_string();
                register_use(pfx, &leaf, fp);
            }
        }
        "use_wildcard" => {
            // `use a::b::*` — register the prefix as a glob source (no alias leaf).
            if let Some(t) = node_text(node, bytes) {
                let t = t.trim_end_matches("::*").to_string();
                let full = norm_crate(&prefixed(pfx, &t), crate_name);
                fp.use_targets.push(full);
            }
        }
        _ => {
            // Unknown wrapper: descend.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                expand_use(&child, bytes, pfx, crate_name, fp);
            }
        }
    }
}

fn register_use(full: &str, leaf: &str, fp: &mut FileParse) {
    fp.use_targets.push(full.to_string());
    fp.alias_map.insert(leaf.to_string(), full.to_string());
}

/// Combine a prefix with a path fragment, avoiding double `::` and handling the
/// empty-prefix case.
fn prefixed(pfx: &str, frag: &str) -> String {
    if pfx.is_empty() {
        frag.to_string()
    } else {
        format!("{pfx}::{frag}")
    }
}

/// Normalize a leading `crate::` to the real crate name.
fn norm_crate(path: &str, crate_name: &str) -> String {
    if let Some(rest) = path.strip_prefix("crate::") {
        format!("{crate_name}::{rest}")
    } else if path == "crate" {
        crate_name.to_string()
    } else {
        path.to_string()
    }
}

// ─── Parsing (tree-sitter-typescript / tsx) ──────────────────────────────────

/// Known source extensions trimmed off TS/JS module stems.
const TS_EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// Parse a TypeScript / TSX / JavaScript file. `.ts`/`.mts`/`.cts` use the
/// TYPESCRIPT grammar; `.tsx`/`.jsx` and the plain-JS flavors use the TSX
/// grammar (JSX parses; the TS-only ambiguities TSX trips on don't occur in JS).
fn parse_typescript_file(file_path: &str, source: &str) -> Option<FileParse> {
    let (pkg_ident, module) = ts_module_for_file(file_path);
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let grammar = if matches!(ext, "ts" | "mts" | "cts") {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT
    } else {
        tree_sitter_typescript::LANGUAGE_TSX
    };
    let mut parser = Parser::new();
    parser.set_language(&grammar.into()).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let bytes = source.as_bytes();

    let mut fp = FileParse {
        lang: Lang::TypeScript,
        module: module.clone(),
        crate_name: pkg_ident,
        symbols: Vec::new(),
        calls: Vec::new(),
        use_targets: Vec::new(),
        alias_map: HashMap::new(),
    };
    walk_ts(&root, bytes, &module, file_path, None, &mut fp);
    // Calls are collected in ONE global pass — attribution is byte-span based
    // (innermost_fn), so per-function collection would only risk double counts.
    collect_ts_calls(&root, bytes, &mut fp);
    Some(fp)
}

fn walk_ts(
    node: &Node,
    bytes: &[u8],
    mod_path: &str,
    file_path: &str,
    parent: Option<usize>,
    fp: &mut FileParse,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_statement" => collect_ts_import(&child, bytes, file_path, fp),
            "export_statement" => {
                // Re-export (`export { x } from './m'` / `export * from './m'`):
                // record the module as an import target, then descend so wrapped
                // declarations (`export function foo` ...) register normally.
                if let Some(src) = child.child_by_field_name("source") {
                    if let Some(t) = string_literal_text(&src, bytes) {
                        fp.use_targets.push(ts_import_module(&t, file_path));
                    }
                }
                walk_ts(&child, bytes, mod_path, file_path, parent, fp);
            }
            "function_declaration" | "generator_function_declaration" => {
                if let Some(name) = child_field_text(&child, "name", bytes) {
                    let idx = fp.symbols.len();
                    fp.symbols.push(Symbol {
                        qualified_name: join(mod_path, &name),
                        node_type: "code:function",
                        start: child.start_byte(),
                        end: child.end_byte(),
                        parent,
                    });
                    walk_ts(&child, bytes, mod_path, file_path, Some(idx), fp);
                }
            }
            "class_declaration" | "abstract_class_declaration" => {
                if let Some(name) = child_field_text(&child, "name", bytes) {
                    let class_path = join(mod_path, &name);
                    let idx = fp.symbols.len();
                    fp.symbols.push(Symbol {
                        qualified_name: class_path.clone(),
                        node_type: "code:class",
                        start: child.start_byte(),
                        end: child.end_byte(),
                        parent,
                    });
                    // Methods qualify under the class: module::Class::method.
                    walk_ts(&child, bytes, &class_path, file_path, Some(idx), fp);
                }
            }
            "interface_declaration" | "enum_declaration" => {
                let nt = if child.kind() == "enum_declaration" {
                    "code:enum"
                } else {
                    "code:interface"
                };
                if let Some(name) = child_field_text(&child, "name", bytes) {
                    fp.symbols.push(Symbol {
                        qualified_name: join(mod_path, &name),
                        node_type: nt,
                        start: child.start_byte(),
                        end: child.end_byte(),
                        parent,
                    });
                }
            }
            "method_definition" => {
                // Inside a class body mod_path is already module::Class.
                if let Some(name) = child_field_text(&child, "name", bytes) {
                    let idx = fp.symbols.len();
                    fp.symbols.push(Symbol {
                        qualified_name: join(mod_path, &name),
                        node_type: "code:function",
                        start: child.start_byte(),
                        end: child.end_byte(),
                        parent,
                    });
                    walk_ts(&child, bytes, mod_path, file_path, Some(idx), fp);
                }
            }
            "variable_declarator" | "public_field_definition" | "field_definition" => {
                ts_declarator(&child, bytes, mod_path, file_path, parent, fp);
            }
            // ERROR-node descent: keep extracting after parse errors.
            _ => walk_ts(&child, bytes, mod_path, file_path, parent, fp),
        }
    }
}

/// `const foo = () => {}` / `bar = function () {}` (incl. class fields) become
/// code:function symbols; `const x = require('./m')` binds a CommonJS alias.
fn ts_declarator(
    node: &Node,
    bytes: &[u8],
    mod_path: &str,
    file_path: &str,
    parent: Option<usize>,
    fp: &mut FileParse,
) {
    let name = node
        .child_by_field_name("name")
        .filter(|n| {
            matches!(
                n.kind(),
                "identifier" | "property_identifier" | "private_property_identifier"
            )
        })
        .and_then(|n| node_text(&n, bytes));
    let Some(value) = node.child_by_field_name("value") else {
        return;
    };
    match value.kind() {
        "arrow_function" | "function_expression" | "function" | "generator_function" => {
            if let Some(name) = name {
                let idx = fp.symbols.len();
                fp.symbols.push(Symbol {
                    qualified_name: join(mod_path, &name),
                    node_type: "code:function",
                    start: node.start_byte(),
                    end: node.end_byte(),
                    parent,
                });
                walk_ts(&value, bytes, mod_path, file_path, Some(idx), fp);
            }
        }
        "call_expression" => {
            // const x = require('./m')
            let is_require = value
                .child_by_field_name("function")
                .and_then(|f| node_text(&f, bytes))
                .is_some_and(|t| t == "require");
            if is_require {
                if let (Some(name), Some(args)) = (name, value.child_by_field_name("arguments")) {
                    let mut c = args.walk();
                    for a in args.children(&mut c) {
                        if a.kind() == "string" {
                            if let Some(src) = string_literal_text(&a, bytes) {
                                register_use(&ts_import_module(&src, file_path), &name, fp);
                            }
                            break;
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Collect an `import ... from '...'` statement into use_targets + alias_map.
fn collect_ts_import(node: &Node, bytes: &[u8], file_path: &str, fp: &mut FileParse) {
    let Some(target) = node
        .child_by_field_name("source")
        .and_then(|s| string_literal_text(&s, bytes))
        .map(|t| ts_import_module(&t, file_path))
    else {
        return;
    };
    let mut bound_any = false;
    let mut cursor = node.walk();
    for clause in node.children(&mut cursor) {
        if clause.kind() != "import_clause" {
            continue;
        }
        let mut cc = clause.walk();
        for c in clause.children(&mut cc) {
            match c.kind() {
                "identifier" => {
                    // default import: `import Foo from './m'` — best-effort bind
                    // Foo -> <m>::Foo (default exports usually share the name).
                    if let Some(name) = node_text(&c, bytes) {
                        register_use(&join(&target, &name), &name, fp);
                        bound_any = true;
                    }
                }
                "namespace_import" => {
                    // `* as ns` — ns aliases the whole module.
                    let mut nc = c.walk();
                    for n in c.children(&mut nc) {
                        if n.kind() == "identifier" {
                            if let Some(name) = node_text(&n, bytes) {
                                register_use(&target, &name, fp);
                                bound_any = true;
                            }
                        }
                    }
                }
                "named_imports" => {
                    let mut nc = c.walk();
                    for spec in c.children(&mut nc) {
                        if spec.kind() != "import_specifier" {
                            continue;
                        }
                        let Some(name) = child_field_text(&spec, "name", bytes) else {
                            continue;
                        };
                        let local =
                            child_field_text(&spec, "alias", bytes).unwrap_or_else(|| name.clone());
                        let full = join(&target, &name);
                        fp.use_targets.push(full.clone());
                        fp.alias_map.insert(local, full);
                        bound_any = true;
                    }
                }
                _ => {}
            }
        }
    }
    if !bound_any {
        // side-effect import (`import './polyfill'`) — still an imports edge.
        fp.use_targets.push(target);
    }
}

/// One global pass: record every call / `new` site with a resolvable path shape.
fn collect_ts_calls(node: &Node, bytes: &[u8], fp: &mut FileParse) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "call_expression" => {
                if let Some(func) = child.child_by_field_name("function") {
                    if let Some(raw) = ts_member_path(&func, bytes) {
                        if raw != "require" && raw != "import" {
                            fp.calls.push(CallSite {
                                raw_path: raw,
                                at: child.start_byte(),
                            });
                        }
                    }
                }
            }
            "new_expression" => {
                if let Some(ctor) = child.child_by_field_name("constructor") {
                    if let Some(path) = ts_member_path(&ctor, bytes) {
                        fp.calls.push(CallSite {
                            raw_path: format!("{path}::constructor"),
                            at: child.start_byte(),
                        });
                    }
                }
            }
            _ => {}
        }
        collect_ts_calls(&child, bytes, fp);
    }
}

/// Dotted member chain -> `::`-joined path. Only simple identifier/this/super
/// chains; computed members, call results etc. are skipped — type inference is
/// out of scope, mirroring the Rust extractor's method-call policy.
fn ts_member_path(node: &Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "property_identifier" | "private_property_identifier" | "this" | "super" => {
            node_text(node, bytes)
        }
        "member_expression" => {
            let obj = ts_member_path(&node.child_by_field_name("object")?, bytes)?;
            let prop = node_text(&node.child_by_field_name("property")?, bytes)?;
            Some(format!("{obj}::{prop}"))
        }
        // unwrap `foo!()` / `(foo)()`
        "non_null_expression" | "parenthesized_expression" => {
            let mut cursor = node.walk();
            let inner = node.children(&mut cursor).find(|c| c.is_named())?;
            ts_member_path(&inner, bytes)
        }
        _ => None,
    }
}

/// Text of a string literal node, without quotes/backticks.
fn string_literal_text(node: &Node, bytes: &[u8]) -> Option<String> {
    let t = node_text(node, bytes)?;
    Some(
        t.trim_matches(|c| c == '"' || c == '\'' || c == '`')
            .to_string(),
    )
}

/// Derive (package_ident, module) for a TS/JS file: nearest package.json's name
/// (scope stripped, sanitized) + the `::`-joined path under that package root
/// (a leading `src` and a trailing `index` collapse, mirroring Node resolution).
fn ts_module_for_file(file_path: &str) -> (String, String) {
    let path = Path::new(file_path);
    let pkg_root = find_pkg_root(path);
    let pkg_ident = pkg_root
        .as_deref()
        .and_then(|r| read_package_json_name(&r.join("package.json")))
        .map(|n| sanitize_ident(n.rsplit('/').next().unwrap_or(&n)))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pkg".to_string());
    let module = ts_module_under_root(pkg_root.as_deref(), path, &pkg_ident);
    (pkg_ident, module)
}

fn ts_module_under_root(root: Option<&Path>, path: &Path, pkg_ident: &str) -> String {
    let mut segs: Vec<String> = Vec::new();
    match root.and_then(|r| path.strip_prefix(r).ok()) {
        Some(rel) => {
            let comps: Vec<_> = rel.components().collect();
            for (i, comp) in comps.iter().enumerate() {
                let s = comp.as_os_str().to_string_lossy().to_string();
                let is_last = i == comps.len() - 1;
                if is_last {
                    let stem = trim_ts_ext(&s);
                    if stem != "index" {
                        segs.push(sanitize_ident(&stem));
                    }
                } else if !(i == 0 && s == "src") {
                    segs.push(sanitize_ident(&s));
                }
            }
        }
        None => {
            // No package root: fall back to the bare file stem.
            if let Some(stem) = path.file_name().map(|s| s.to_string_lossy().to_string()) {
                let stem = trim_ts_ext(&stem);
                if stem != "index" {
                    segs.push(sanitize_ident(&stem));
                }
            }
        }
    }
    let mut module = pkg_ident.to_string();
    for s in segs {
        module = join(&module, &s);
    }
    module
}

/// Strip one known TS/JS extension (and a preceding `.d`) off a file name.
fn trim_ts_ext(name: &str) -> String {
    for ext in TS_EXTS {
        if let Some(stem) = name.strip_suffix(&format!(".{ext}")) {
            let stem = stem.strip_suffix(".d").unwrap_or(stem);
            return stem.to_string();
        }
    }
    name.to_string()
}

/// Keep [A-Za-z0-9_], map everything else to `_` (so `Button.test` and
/// `my-dir` stay single `::` segments).
fn sanitize_ident(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn find_pkg_root(path: &Path) -> Option<PathBuf> {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.join("package.json").is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

fn read_package_json_name(pkg_json: &Path) -> Option<String> {
    let text = std::fs::read_to_string(pkg_json).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("name")?.as_str().map(|s| s.to_string())
}

/// Resolve an import source string to a module path. Relative sources resolve
/// against the importing file's directory with the SAME path math as
/// ts_module_for_file (so internal imports land on real symbol modules); bare
/// package specifiers become `pkg::subpath` externs.
fn ts_import_module(source: &str, file_path: &str) -> String {
    if source.starts_with('.') {
        let dir = Path::new(file_path)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let mut parts: Vec<std::ffi::OsString> = dir
            .components()
            .map(|c| c.as_os_str().to_os_string())
            .collect();
        for seg in source.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                s => parts.push(s.into()),
            }
        }
        let mut target = PathBuf::new();
        for p in &parts {
            target.push(p);
        }
        let (_, module) = ts_module_for_file(&target.to_string_lossy());
        module
    } else {
        // bare specifier: '@scope/pkg/sub' -> scope::pkg::sub (extern)
        let mut out = String::new();
        for seg in source.trim_start_matches('@').split('/') {
            if seg.is_empty() {
                continue;
            }
            let seg = sanitize_ident(seg);
            out = join(&out, &seg);
        }
        if out.is_empty() {
            "extern".to_string()
        } else {
            out
        }
    }
}

// ─── Parsing (tree-sitter-java) ──────────────────────────────────────────────

/// Parse a Java file. Module = the `package` declaration (dots -> `::`);
/// classes/interfaces/enums/records nest (module::Outer::Inner) and methods/
/// constructors are code:function under their type.
fn parse_java_file(_file_path: &str, source: &str) -> Option<FileParse> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let bytes = source.as_bytes();

    // package a.b.c; -> module a::b::c (no package -> names start at the class).
    let mut module = String::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "package_declaration" {
            let mut pc = child.walk();
            for p in child.children(&mut pc) {
                if matches!(p.kind(), "identifier" | "scoped_identifier") {
                    if let Some(t) = node_text(&p, bytes) {
                        module = t.replace('.', "::");
                    }
                }
            }
            break;
        }
    }

    let mut fp = FileParse {
        lang: Lang::Java,
        module: module.clone(),
        crate_name: module.clone(),
        symbols: Vec::new(),
        calls: Vec::new(),
        use_targets: Vec::new(),
        alias_map: HashMap::new(),
    };
    walk_java(&root, bytes, &module, None, &mut fp);
    // One global call pass (byte-span attribution via innermost_fn).
    collect_java_calls(&root, bytes, &mut fp);
    Some(fp)
}

fn walk_java(node: &Node, bytes: &[u8], mod_path: &str, parent: Option<usize>, fp: &mut FileParse) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_declaration" => collect_java_import(&child, bytes, fp),
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration" => {
                let nt = match child.kind() {
                    "class_declaration" => "code:class",
                    "interface_declaration" | "annotation_type_declaration" => "code:interface",
                    "enum_declaration" => "code:enum",
                    _ => "code:struct", // record: a data carrier
                };
                if let Some(name) = child_field_text(&child, "name", bytes) {
                    let type_path = join(mod_path, &name);
                    let idx = fp.symbols.len();
                    fp.symbols.push(Symbol {
                        qualified_name: type_path.clone(),
                        node_type: nt,
                        start: child.start_byte(),
                        end: child.end_byte(),
                        parent,
                    });
                    // Members qualify under the type: module::Class::method.
                    walk_java(&child, bytes, &type_path, Some(idx), fp);
                }
            }
            "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration" => {
                if let Some(name) = child_field_text(&child, "name", bytes) {
                    let idx = fp.symbols.len();
                    fp.symbols.push(Symbol {
                        qualified_name: join(mod_path, &name),
                        node_type: "code:function",
                        start: child.start_byte(),
                        end: child.end_byte(),
                        parent,
                    });
                    // local classes inside bodies still register (same module).
                    walk_java(&child, bytes, mod_path, Some(idx), fp);
                }
            }
            // ERROR-node descent + generic recursion (bodies, modifiers, ...).
            _ => walk_java(&child, bytes, mod_path, parent, fp),
        }
    }
}

/// `import a.b.C;`, `import static a.b.C.m;`, `import a.b.*;`
fn collect_java_import(node: &Node, bytes: &[u8], fp: &mut FileParse) {
    let mut path_text: Option<String> = None;
    let mut wildcard = false;
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        match c.kind() {
            "identifier" | "scoped_identifier" => path_text = node_text(&c, bytes),
            "asterisk" => wildcard = true,
            _ => {}
        }
    }
    let Some(t) = path_text else { return };
    let full = t.replace('.', "::");
    if wildcard {
        // `import a.b.*;` — record the package, no leaf alias to bind.
        fp.use_targets.push(full);
    } else {
        let leaf = full.rsplit("::").next().unwrap_or(&full).to_string();
        register_use(&full, &leaf, fp);
    }
}

/// One global pass over the tree: method invocations + constructor calls.
fn collect_java_calls(node: &Node, bytes: &[u8], fp: &mut FileParse) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "method_invocation" => {
                if let Some(name) = child_field_text(&child, "name", bytes) {
                    let raw = match child.child_by_field_name("object") {
                        None => Some(name),
                        Some(obj) => {
                            java_receiver_path(&obj, bytes).map(|o| format!("{o}::{name}"))
                        }
                    };
                    if let Some(raw) = raw {
                        fp.calls.push(CallSite {
                            raw_path: raw,
                            at: child.start_byte(),
                        });
                    }
                }
            }
            "object_creation_expression" => {
                // `new Foo(...)` -> a call to the constructor Foo::Foo.
                if let Some(ty) = child
                    .child_by_field_name("type")
                    .and_then(|t| java_type_path(&t, bytes))
                {
                    let leaf = ty.rsplit("::").next().unwrap_or(&ty).to_string();
                    fp.calls.push(CallSite {
                        raw_path: format!("{ty}::{leaf}"),
                        at: child.start_byte(),
                    });
                }
            }
            _ => {}
        }
        collect_java_calls(&child, bytes, fp);
    }
}

/// Receiver chains we can express without type inference: identifiers, this/
/// super, and field-access chains of those. Anything else (call results,
/// casts, array elements) is skipped.
fn java_receiver_path(node: &Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "this" | "super" => node_text(node, bytes),
        "field_access" => {
            let obj = java_receiver_path(&node.child_by_field_name("object")?, bytes)?;
            let field = node_text(&node.child_by_field_name("field")?, bytes)?;
            Some(format!("{obj}::{field}"))
        }
        _ => None,
    }
}

/// Type node of a `new` expression -> `::` path (generics stripped).
fn java_type_path(node: &Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" => node_text(node, bytes),
        "scoped_type_identifier" => node_text(node, bytes).map(|t| t.replace('.', "::")),
        "generic_type" => {
            let mut cursor = node.walk();
            let inner = node
                .children(&mut cursor)
                .find(|c| matches!(c.kind(), "type_identifier" | "scoped_type_identifier"))?;
            java_type_path(&inner, bytes)
        }
        _ => None,
    }
}

// ─── Parsing (tree-sitter-python) ────────────────────────────────────────────

/// Parse a Python file. Module = the file's package path (dotted dirs that carry
/// an `__init__.py`, then the file stem; `__init__.py` collapses to its package).
/// `class` bodies nest (module::Class), `def`s become code:function under their
/// enclosing class or the module. Call resolution is the dotty form shared with
/// TS/Java: imported names (`from m import f` → bare `f()`) bind through the alias
/// map, `self` plays the role of the enclosing class, unknown receivers are kept
/// verbatim so the extern node still answers bare-leaf `callers_of` queries.
fn parse_python_file(file_path: &str, source: &str) -> Option<FileParse> {
    let (crate_name, module) = python_module_for_file(file_path);

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let bytes = source.as_bytes();

    let mut fp = FileParse {
        lang: Lang::Python,
        module: module.clone(),
        crate_name,
        symbols: Vec::new(),
        calls: Vec::new(),
        use_targets: Vec::new(),
        alias_map: HashMap::new(),
    };
    walk_python(&root, bytes, &module, None, &mut fp);
    // One global call pass (byte-span attribution via innermost_fn).
    collect_python_calls(&root, bytes, &mut fp);
    Some(fp)
}

fn walk_python(
    node: &Node,
    bytes: &[u8],
    mod_path: &str,
    parent: Option<usize>,
    fp: &mut FileParse,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_statement" | "import_from_statement" => {
                collect_python_import(&child, bytes, fp)
            }
            "class_definition" => {
                if let Some(name) = child_field_text(&child, "name", bytes) {
                    let type_path = join(mod_path, &name);
                    let idx = fp.symbols.len();
                    fp.symbols.push(Symbol {
                        qualified_name: type_path.clone(),
                        node_type: "code:class",
                        start: child.start_byte(),
                        end: child.end_byte(),
                        parent,
                    });
                    // Methods qualify under the class: module::Class::method.
                    walk_python(&child, bytes, &type_path, Some(idx), fp);
                }
            }
            "function_definition" => {
                if let Some(name) = child_field_text(&child, "name", bytes) {
                    let idx = fp.symbols.len();
                    fp.symbols.push(Symbol {
                        qualified_name: join(mod_path, &name),
                        node_type: "code:function",
                        start: child.start_byte(),
                        end: child.end_byte(),
                        parent,
                    });
                    // Nested defs inside the body keep the same module path.
                    walk_python(&child, bytes, mod_path, Some(idx), fp);
                }
            }
            // decorated_definition / block / etc.: descend, same scope.
            _ => walk_python(&child, bytes, mod_path, parent, fp),
        }
    }
}

/// `import a.b.c`, `import a.b as c`, `from a.b import c, d as e`, `from . import x`,
/// `from a import *`. Imported leaf names bind in the alias map so a later bare
/// `c()` resolves to `a::b::c`; a plain `import a.b.c` only records the use target
/// (Python binds the head package `a`, which we leave as an extern receiver).
fn collect_python_import(node: &Node, bytes: &[u8], fp: &mut FileParse) {
    let mut cursor = node.walk();
    if node.kind() == "import_statement" {
        for c in node.children(&mut cursor) {
            match c.kind() {
                "dotted_name" => {
                    if let Some(t) = node_text(&c, bytes) {
                        fp.use_targets.push(t.replace('.', "::"));
                    }
                }
                "aliased_import" => {
                    // `import a.b as c` — bind the alias to the full path.
                    let full = c
                        .child_by_field_name("name")
                        .and_then(|n| node_text(&n, bytes))
                        .map(|t| t.replace('.', "::"));
                    let alias = child_field_text(&c, "alias", bytes);
                    if let (Some(full), Some(alias)) = (full, alias) {
                        fp.use_targets.push(full.clone());
                        fp.alias_map.insert(alias, full);
                    }
                }
                _ => {}
            }
        }
        return;
    }

    // import_from_statement: `from <module_name> import <names>`.
    let base = node
        .child_by_field_name("module_name")
        .and_then(|m| python_module_name(&m, bytes))
        .unwrap_or_default();
    for c in node.children(&mut cursor) {
        // Skip the module_name node itself (it is the source, not an imported name).
        if Some(c.id()) == node.child_by_field_name("module_name").map(|m| m.id()) {
            continue;
        }
        match c.kind() {
            "dotted_name" | "identifier" => {
                if let Some(name) = node_text(&c, bytes) {
                    let leaf = name.replace('.', "::");
                    let full = join(&base, &leaf);
                    register_use(&full, &leaf, fp);
                }
            }
            "aliased_import" => {
                let name = c
                    .child_by_field_name("name")
                    .and_then(|n| node_text(&n, bytes))
                    .map(|t| t.replace('.', "::"));
                let alias = child_field_text(&c, "alias", bytes);
                if let (Some(name), Some(alias)) = (name, alias) {
                    let full = join(&base, &name);
                    fp.use_targets.push(full.clone());
                    fp.alias_map.insert(alias, full);
                }
            }
            "wildcard_import" => {
                if !base.is_empty() {
                    fp.use_targets.push(base.clone());
                }
            }
            _ => {}
        }
    }
}

/// `module_name` of a from-import: a `dotted_name` (`a.b`) or a `relative_import`
/// (`.`, `..mod`). Relative dots are dropped — we keep just the named tail so a
/// relative import still binds the leaf without inventing a package path.
fn python_module_name(node: &Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "dotted_name" => node_text(node, bytes).map(|t| t.replace('.', "::")),
        "relative_import" => {
            // `relative_import` = import_prefix (dots) + optional dotted_name.
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .find(|c| c.kind() == "dotted_name")
                .and_then(|c| node_text(&c, bytes))
                .map(|t| t.replace('.', "::"))
        }
        _ => None,
    }
}

/// One global pass over the tree: every `call` node, attributed to its enclosing
/// function by byte span in the second pass. `f()` → `f`; `self.f()` → `self::f`;
/// `a.b.c()` → `a::b::c`. Receivers we cannot express as a plain ident/attribute
/// chain (call results, subscripts) are skipped.
fn collect_python_calls(node: &Node, bytes: &[u8], fp: &mut FileParse) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "call" {
            if let Some(func) = child.child_by_field_name("function") {
                if let Some(raw) = python_callee_path(&func, bytes) {
                    fp.calls.push(CallSite {
                        raw_path: raw,
                        at: child.start_byte(),
                    });
                }
            }
        }
        collect_python_calls(&child, bytes, fp);
    }
}

/// Turn a call's `function` node into a `::`-joined path. Identifiers and
/// attribute chains of identifiers (`a.b.c`) resolve; anything else (a call
/// result `foo().bar`, a subscript `d[k].m`) yields `None`.
fn python_callee_path(node: &Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node_text(node, bytes),
        "attribute" => {
            let obj = python_callee_path(&node.child_by_field_name("object")?, bytes)?;
            let attr = node_text(&node.child_by_field_name("attribute")?, bytes)?;
            Some(format!("{obj}::{attr}"))
        }
        _ => None,
    }
}

/// Derive (crate_name, module) for a Python file from its package path: ancestor
/// directories that carry an `__init__.py` are package segments (top-down), then
/// the file stem (`__init__.py` collapses to its package). With no package on
/// disk the module is just the file stem — which is what the unit tests exercise.
fn python_module_for_file(file_path: &str) -> (String, String) {
    let path = Path::new(file_path);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "module".to_string());

    let mut pkgs: Vec<String> = Vec::new();
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.join("__init__.py").is_file() {
            if let Some(name) = d.file_name() {
                pkgs.push(name.to_string_lossy().to_string());
            }
            dir = d.parent();
        } else {
            break;
        }
    }
    pkgs.reverse(); // top package first

    let mut segs = pkgs;
    if stem != "__init__" {
        segs.push(stem.clone());
    }
    let module = if segs.is_empty() {
        stem.clone()
    } else {
        segs.join("::")
    };
    let crate_name = segs.into_iter().next().unwrap_or(stem);
    (crate_name, module)
}

// ─── Call resolution (THE DIFFERENTIATOR) ────────────────────────────────────

/// Resolve a raw call path to a fully-qualified name, given the enclosing fn's
/// qualified name (`caller_qn`) and the file's parse (module / crate / aliases).
pub(crate) fn resolve_call(raw: &str, caller_qn: &str, fp: &FileParse) -> String {
    match fp.lang {
        Lang::Rust => resolve_call_inner(raw, caller_qn, &fp.module, &fp.crate_name, &fp.alias_map),
        Lang::TypeScript | Lang::Java | Lang::Python => resolve_call_dotty(raw, caller_qn, fp),
    }
}

/// TS/Java call resolution. Differences from Rust: bare calls check the alias
/// map FIRST (imported functions / static imports are THE dominant call form),
/// `this` plays the role of `self` (the caller's enclosing class), Upper-case
/// heads fall back to a same-module class, and unknown lower-case receivers
/// (instance vars, globals, fully-qualified package paths) are kept as written
/// so the extern node still matches bare-leaf `callers_of` queries.
fn resolve_call_dotty(raw: &str, caller_qn: &str, fp: &FileParse) -> String {
    // The caller's own scope = caller_qn minus its leaf (the fn/method name):
    // for a method that is the enclosing class (module::Class); for a free fn,
    // the module.
    let caller_module = caller_qn
        .rsplit_once("::")
        .map(|(m, _)| m.to_string())
        .unwrap_or_else(|| fp.module.clone());

    if !raw.contains("::") {
        if let Some(full) = fp.alias_map.get(raw) {
            return full.clone();
        }
        return join(&caller_module, raw);
    }

    let (head, rest) = raw.split_once("::").expect("checked contains above");
    match head {
        "this" | "self" => join(&caller_module, rest),
        "super" => {
            let parent = caller_module
                .rsplit_once("::")
                .map(|(p, _)| p.to_string())
                .unwrap_or_else(|| fp.crate_name.clone());
            join(&parent, rest)
        }
        _ => {
            if let Some(full) = fp.alias_map.get(head) {
                join(full, rest)
            } else if head.chars().next().is_some_and(|c| c.is_uppercase()) {
                // Class-ish receiver with no import: same-module type
                // (static call or constructor).
                join(&fp.module, raw)
            } else {
                // Unknown receiver — keep as written (becomes code:extern).
                raw.to_string()
            }
        }
    }
}

fn resolve_call_inner(
    raw: &str,
    caller_qn: &str,
    module: &str,
    crate_name: &str,
    alias_map: &HashMap<String, String>,
) -> String {
    // The caller's own module = caller_qn minus its leaf (the fn name).
    let caller_module = caller_qn
        .rsplit_once("::")
        .map(|(m, _)| m.to_string())
        .unwrap_or_else(|| module.to_string());

    // bare identifier (no ::) -> caller's module.
    if !raw.contains("::") {
        return join(&caller_module, raw);
    }

    let segs: Vec<&str> = raw.split("::").collect();
    let head = segs[0];
    let rest = segs[1..].join("::");

    match head {
        "self" => join(&caller_module, &rest),
        "super" => {
            let parent = caller_module
                .rsplit_once("::")
                .map(|(p, _)| p.to_string())
                .unwrap_or_else(|| crate_name.to_string());
            join(&parent, &rest)
        }
        "crate" => join(crate_name, &rest),
        // alias::rest -> expand the alias to its full module path.
        other => {
            if let Some(full) = alias_map.get(other) {
                join(full, &rest)
            } else {
                // Could be a sibling submodule of the caller's module, or an
                // already-qualified external/std path. Heuristic: if the head
                // looks like a crate (matches our crate name) keep as-is; if it
                // matches a known top-level (has more than one segment AND looks
                // external) keep as-is; otherwise treat as caller-module-relative.
                if head == crate_name || segs.len() >= 3 || looks_external(head) {
                    norm_crate(raw, crate_name)
                } else {
                    join(&caller_module, raw)
                }
            }
        }
    }
}

/// External / std crates are treated as already-qualified.
fn looks_external(head: &str) -> bool {
    matches!(
        head,
        "std"
            | "core"
            | "alloc"
            | "tokio"
            | "serde"
            | "serde_json"
            | "anyhow"
            | "sqlx"
            | "uuid"
            | "chrono"
            | "tracing"
            | "reqwest"
            | "futures"
            | "regex"
            | "redis"
            | "tree_sitter"
            | "tree_sitter_rust"
    )
}

// ─── path / module derivation ────────────────────────────────────────────────

/// Join two path fragments with `::`, skipping empties.
fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else {
        format!("{a}::{b}")
    }
}

fn node_text(node: &Node, bytes: &[u8]) -> Option<String> {
    node.utf8_text(bytes).ok().map(|s| s.to_string())
}

fn child_field_text(node: &Node, field: &str, bytes: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|n| node_text(&n, bytes))
}

/// Derive (crate_name, module_prefix) for a file path. The crate name is the
/// `[package].name` of the nearest ancestor Cargo.toml; the module prefix is the
/// crate name plus the path under `src/` (lib.rs/mod.rs/main.rs collapse to the
/// dir module). Falls back gracefully when no Cargo.toml is found.
fn module_for_file(file_path: &str) -> (String, String) {
    let path = Path::new(file_path);
    let crate_root = find_crate_root(path);
    let crate_name = crate_root
        .as_ref()
        .and_then(|root| read_package_name(&root.join("Cargo.toml")))
        .unwrap_or_else(|| "crate".to_string());
    let crate_ident = crate_name.replace('-', "_");

    let module = match &crate_root {
        Some(root) => {
            let src = root.join("src");
            let rel = path.strip_prefix(&src).ok();
            match rel {
                Some(rel) => {
                    let mut segs: Vec<String> = Vec::new();
                    let comps: Vec<_> = rel.components().collect();
                    for (i, comp) in comps.iter().enumerate() {
                        let s = comp.as_os_str().to_string_lossy().to_string();
                        let is_last = i == comps.len() - 1;
                        if is_last {
                            // file stem; lib/mod/main collapse to nothing.
                            let stem = s.trim_end_matches(".rs");
                            if !matches!(stem, "lib" | "mod" | "main") {
                                segs.push(stem.to_string());
                            }
                        } else {
                            segs.push(s);
                        }
                    }
                    let mut module = crate_ident.clone();
                    for s in segs {
                        module = join(&module, &s);
                    }
                    module
                }
                None => crate_ident.clone(),
            }
        }
        None => crate_ident.clone(),
    };

    (crate_ident, module)
}

fn find_crate_root(path: &Path) -> Option<PathBuf> {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.join("Cargo.toml").is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

fn read_package_name(cargo_toml: &Path) -> Option<String> {
    let text = std::fs::read_to_string(cargo_toml).ok()?;
    let mut in_package = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_package = t == "[package]";
            continue;
        }
        if in_package {
            if let Some(rest) = t.strip_prefix("name") {
                let rest = rest.trim_start().strip_prefix('=')?.trim();
                let name = rest.trim_matches(|c| c == '"' || c == '\'').to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_like_neutralizes_wildcards() {
        // A literal underscore in a Rust name must stay literal, not match-any.
        assert_eq!(escape_like("load_model"), r"load\_model");
        // % and the escape char itself are escaped too; plain names pass through.
        assert_eq!(escape_like("a%b_c\\d"), r"a\%b\_c\\d");
        assert_eq!(escape_like("plainName"), "plainName");
        assert_eq!(escape_like(""), "");
    }

    #[test]
    fn kind_filter_maps_leaves_and_type_alias() {
        // A single leaf kind selects exactly that code:<kind>.
        assert_eq!(kind_filter_types("function"), Some(vec!["code:function"]));
        assert_eq!(kind_filter_types("class"), Some(vec!["code:class"]));
        // Friendly synonyms resolve to the same leaf.
        assert_eq!(kind_filter_types("fn"), kind_filter_types("function"));
        assert_eq!(kind_filter_types("module"), kind_filter_types("mod"));
        // `type` is the cross-language alias for the type-defining symbols.
        assert_eq!(
            kind_filter_types("type"),
            Some(vec![
                "code:struct",
                "code:enum",
                "code:trait",
                "code:class",
                "code:interface",
            ])
        );
        // Every returned node_type is a real code:* leaf.
        for t in kind_filter_types("type").unwrap() {
            assert!(t.starts_with("code:"));
        }
        // Unknown keyword → None (the caller turns this into a loud error).
        assert_eq!(kind_filter_types("widget"), None);
        assert_eq!(kind_filter_types(""), None);
    }

    #[test]
    fn resolve_kind_filter_validates() {
        // No filter stays None (matches everything).
        assert!(resolve_kind_filter(None).unwrap().is_none());
        // A known kind yields owned node_type strings for the text[] bind.
        assert_eq!(
            resolve_kind_filter(Some("function")).unwrap(),
            Some(vec!["code:function".to_string()])
        );
        // An unknown kind errors rather than silently matching nothing.
        assert!(resolve_kind_filter(Some("nope")).is_err());
    }

    #[test]
    fn similarity_is_bounded_and_monotonic() {
        // Exact hit (distance 0) → max similarity.
        assert_eq!(similarity_from_distance(0.0), 1.0);
        // Monotonically decreasing as distance grows; always in (0, 1].
        let near = similarity_from_distance(0.5);
        let far = similarity_from_distance(5.0);
        assert!(near > far, "closer must score higher: {near} vs {far}");
        assert!(far > 0.0 && near <= 1.0);
        // Negative distance can't push the score above 1 (defensive clamp).
        assert_eq!(similarity_from_distance(-0.1), 1.0);
    }

    #[test]
    fn is_test_file_matches_cross_language_conventions() {
        // Directory conventions.
        assert!(is_test_file("crates/ff-db/tests/migrations.rs"));
        assert!(is_test_file("src/__tests__/button.ts"));
        assert!(is_test_file("app/spec/models/user.rb"));
        // Rust unit/integration basenames.
        assert!(is_test_file("crates/ff-brain/src/cortex_test.rs"));
        assert!(is_test_file("src/router_tests.rs"));
        // JS / TS.
        assert!(is_test_file("dashboard/src/Login.test.tsx"));
        assert!(is_test_file("web/auth.spec.js"));
        // Python.
        assert!(is_test_file("svc/test_router.py"));
        assert!(is_test_file("svc/router_test.py"));
        // Java.
        assert!(is_test_file("src/test/java/com/x/UserServiceTest.java"));
        assert!(is_test_file("ConsentSpec.java"));
        // Non-tests stay out.
        assert!(!is_test_file("crates/ff-brain/src/cortex.rs"));
        assert!(!is_test_file("dashboard/src/Login.tsx"));
        assert!(!is_test_file("svc/router.py"));
        assert!(!is_test_file("src/main/java/com/x/UserService.java"));
        // Case-insensitive.
        assert!(is_test_file("SRC/Tests/Foo.RS".to_string().as_str()));
    }

    #[test]
    fn is_test_symbol_uses_file_and_name_conventions() {
        // Test-file path wins regardless of name.
        assert!(is_test_symbol(
            "ff_brain::router::helper",
            Some("crates/ff-brain/tests/router.rs")
        ));
        // Rust inline `#[cfg(test)] mod tests` shows up as `::tests::` in the path,
        // even when the owning file is ordinary source.
        assert!(is_test_symbol(
            "ff_brain::cortex::tests::escape_like_neutralizes_wildcards",
            Some("crates/ff-brain/src/cortex.rs")
        ));
        // `test_`-prefixed leaf (rust/python), non-test file.
        assert!(is_test_symbol("svc.router.test_route_picks_host", None));
        // A normal symbol in a normal file is not a test.
        assert!(!is_test_symbol(
            "ff_brain::cortex::find_symbols",
            Some("crates/ff-brain/src/cortex.rs")
        ));
        // `testing` is not `test_` — no false positive on the prefix.
        assert!(!is_test_symbol("ff_core::testing_utils", None));
    }

    #[test]
    fn risk_tier_thresholds() {
        // Wide transitive reach → high regardless of external count.
        assert_eq!(risk_tier(10, 0), RiskTier::High);
        assert_eq!(risk_tier(50, 0), RiskTier::High);
        // Many cross-file callers → high even with a shallow closure.
        assert_eq!(risk_tier(0, 5), RiskTier::High);
        // Moderate reach OR any external dependent → medium.
        assert_eq!(risk_tier(3, 0), RiskTier::Medium);
        assert_eq!(risk_tier(0, 1), RiskTier::Medium);
        assert_eq!(risk_tier(9, 4), RiskTier::Medium);
        // Nothing external + tiny closure → low (e.g. a brand-new helper).
        assert_eq!(risk_tier(0, 0), RiskTier::Low);
        assert_eq!(risk_tier(2, 0), RiskTier::Low);
    }

    #[test]
    fn hunk_new_span_parses_count_and_default() {
        assert_eq!(parse_hunk_new_span(" -1,4 +10,3 @@ fn foo"), Some((10, 3)));
        assert_eq!(parse_hunk_new_span(" -5 +12 @@"), Some((12, 1))); // no count ⇒ 1
        assert_eq!(parse_hunk_new_span(" -1,2 +0,0 @@"), Some((0, 0))); // pure deletion
        assert_eq!(parse_hunk_new_span(" -1,2 @@"), None); // no + side
    }

    #[test]
    fn diff_line_ranges_extracts_new_side() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,2 +10,3 @@ fn alpha() {
+    let x = 1;
@@ -40,0 +42,5 @@ fn beta() {
+    more();
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -7,3 +7,0 @@ fn gone() {
";
        let m = parse_diff_line_ranges(diff);
        // a.rs: +10,3 → 10..=12 ; +42,5 → 42..=46
        assert_eq!(m.get("src/a.rs").unwrap(), &vec![(10, 12), (42, 46)]);
        // b.rs: pure deletion +7,0 → still flags line 7 (enclosing symbol).
        assert_eq!(m.get("src/b.rs").unwrap(), &vec![(7, 7)]);
    }

    #[test]
    fn diff_line_ranges_skips_dev_null_target() {
        // A fully deleted file (+++ /dev/null) yields no entry.
        let diff = "\
--- a/src/dead.rs
+++ /dev/null
@@ -1,3 +0,0 @@
";
        assert!(parse_diff_line_ranges(diff).is_empty());
    }

    #[test]
    fn ext_lang_maps_known_extensions() {
        assert_eq!(ext_lang("rs"), Some("rust"));
        assert_eq!(ext_lang("tsx"), Some("typescript"));
        assert_eq!(ext_lang("java"), Some("java"));
        assert_eq!(ext_lang("md"), None);
    }

    #[test]
    fn risk_tier_rank_orders_high_first() {
        assert!(RiskTier::High.rank() > RiskTier::Medium.rank());
        assert!(RiskTier::Medium.rank() > RiskTier::Low.rank());
    }

    #[test]
    fn byte_to_line_maps_offsets() {
        // bytes: a(0) b(1) \n(2) c(3) d(4) \n(5) \n(6) e(7) f(8)
        // lines: 1=ab[0..2] 2=cd[3..5] 3=blank[6] 4=ef[7..8]
        let src = "ab\ncd\n\nef";
        let ls = line_start_offsets(src);
        assert_eq!(byte_to_line(&ls, 0), 1); // 'a'
        assert_eq!(byte_to_line(&ls, 1), 1); // 'b'
        assert_eq!(byte_to_line(&ls, 2), 1); // '\n' belongs to its line
        assert_eq!(byte_to_line(&ls, 3), 2); // 'c'
        assert_eq!(byte_to_line(&ls, 6), 3); // the blank line's '\n'
        assert_eq!(byte_to_line(&ls, 7), 4); // 'e'
        assert_eq!(byte_to_line(&ls, 8), 4); // 'f'
    }

    #[test]
    fn symbol_touched_by_hunks_overlap() {
        // fn spanning lines 10..=20.
        let (s, e) = (Some(10), Some(20));
        assert!(symbol_touched_by_hunks(s, e, &[(15, 15)])); // change inside
        assert!(symbol_touched_by_hunks(s, e, &[(5, 12)])); // overlaps start
        assert!(symbol_touched_by_hunks(s, e, &[(18, 30)])); // overlaps end
        assert!(symbol_touched_by_hunks(s, e, &[(10, 20)])); // exact
        assert!(!symbol_touched_by_hunks(s, e, &[(1, 9)])); // strictly before
        assert!(!symbol_touched_by_hunks(s, e, &[(21, 40)])); // strictly after
        assert!(symbol_touched_by_hunks(s, e, &[(1, 9), (21, 22), (20, 25)])); // any-of
    }

    #[test]
    fn symbol_touched_by_hunks_fails_open_on_missing_span() {
        // No recorded span (pre-V124 node) ⇒ always kept, never hidden.
        assert!(symbol_touched_by_hunks(None, None, &[(1, 2)]));
        assert!(symbol_touched_by_hunks(Some(10), None, &[(1, 2)]));
        assert!(symbol_touched_by_hunks(None, Some(10), &[(1, 2)]));
        // Empty hunk list with a known span ⇒ not touched.
        assert!(!symbol_touched_by_hunks(Some(10), Some(20), &[]));
    }

    fn fr(path: &str, hash: &str) -> (String, FileRow) {
        (
            "rust".to_string(),
            FileRow {
                id: Uuid::nil(),
                path: path.to_string(),
                content_hash: hash.to_string(),
            },
        )
    }

    #[test]
    fn partition_empty_ledger_marks_everything_changed() {
        // First run: nothing tracked → every file is changed/new, none unchanged.
        let tracked = HashMap::new();
        let current = vec![fr("a.rs", "h1"), fr("b.rs", "h2")];
        let (changed, unchanged, deleted) = partition_changes(&tracked, &current);
        assert_eq!(changed.len(), 2);
        assert_eq!(unchanged, 0);
        assert!(deleted.is_empty());
    }

    #[test]
    fn partition_detects_changed_unchanged_new() {
        let tracked = HashMap::from([
            ("a.rs".to_string(), "h1".to_string()),  // unchanged
            ("b.rs".to_string(), "OLD".to_string()), // changed
        ]);
        let current = vec![
            fr("a.rs", "h1"), // same hash → unchanged
            fr("b.rs", "h2"), // hash differs → changed
            fr("c.rs", "h3"), // not tracked → new (changed)
        ];
        let (changed, unchanged, deleted) = partition_changes(&tracked, &current);
        let changed_paths: HashSet<&str> = changed.iter().map(|(_, f)| f.path.as_str()).collect();
        assert_eq!(unchanged, 1);
        assert!(changed_paths.contains("b.rs"));
        assert!(changed_paths.contains("c.rs"));
        assert!(!changed_paths.contains("a.rs"));
        assert!(deleted.is_empty());
    }

    #[test]
    fn partition_flags_deleted_files() {
        // d.rs was tracked but is gone from the current scan → deleted.
        let tracked = HashMap::from([
            ("a.rs".to_string(), "h1".to_string()),
            ("d.rs".to_string(), "hd".to_string()),
        ]);
        let current = vec![fr("a.rs", "h1")];
        let (changed, unchanged, deleted) = partition_changes(&tracked, &current);
        assert!(changed.is_empty());
        assert_eq!(unchanged, 1);
        assert_eq!(deleted, vec!["d.rs".to_string()]);
    }

    fn fp_with(module: &str, crate_name: &str, aliases: &[(&str, &str)]) -> FileParse {
        let mut alias_map = HashMap::new();
        for (k, v) in aliases {
            alias_map.insert(k.to_string(), v.to_string());
        }
        FileParse {
            lang: Lang::Rust,
            module: module.to_string(),
            crate_name: crate_name.to_string(),
            symbols: vec![],
            calls: vec![],
            use_targets: vec![],
            alias_map,
        }
    }

    #[test]
    fn bare_call_resolves_to_caller_module() {
        // model_runtime.rs:712 `load_model(` inside fn resume_local_models.
        let fp = fp_with("ff_agent::model_runtime", "ff_agent", &[]);
        let got = resolve_call(
            "load_model",
            "ff_agent::model_runtime::resume_local_models",
            &fp,
        );
        assert_eq!(got, "ff_agent::model_runtime::load_model");
    }

    #[test]
    fn crate_prefixed_call_normalizes_crate() {
        // deployment_reconciler.rs:296 `crate::model_runtime::load_model(`.
        let fp = fp_with("ff_agent::deployment_reconciler", "ff_agent", &[]);
        let got = resolve_call(
            "crate::model_runtime::load_model",
            "ff_agent::deployment_reconciler::respawn_dead_deployment",
            &fp,
        );
        assert_eq!(got, "ff_agent::model_runtime::load_model");
    }

    #[test]
    fn use_self_alias_resolves_cross_module() {
        // autoscaler.rs:547 `model_runtime::load_model(` with
        // `use crate::model_runtime;` (alias model_runtime -> ff_agent::model_runtime).
        let fp = fp_with(
            "ff_agent::autoscaler",
            "ff_agent",
            &[("model_runtime", "ff_agent::model_runtime")],
        );
        let got = resolve_call(
            "model_runtime::load_model",
            "ff_agent::autoscaler::do_load",
            &fp,
        );
        assert_eq!(got, "ff_agent::model_runtime::load_model");
    }

    #[test]
    fn cross_crate_alias_resolves() {
        // model_cmd.rs:681/:774 `ff_agent::model_runtime::load_model(` from
        // ff-terminal. Head is the external crate name -> already qualified.
        let fp = fp_with("ff_terminal::model_cmd", "ff_terminal", &[]);
        let got = resolve_call(
            "ff_agent::model_runtime::load_model",
            "ff_terminal::model_cmd::handle_model",
            &fp,
        );
        assert_eq!(got, "ff_agent::model_runtime::load_model");
    }

    #[test]
    fn self_and_super_resolve() {
        let fp = fp_with("ff_agent::deep::inner", "ff_agent", &[]);
        assert_eq!(
            resolve_call("self::helper", "ff_agent::deep::inner::run", &fp),
            "ff_agent::deep::inner::helper"
        );
        assert_eq!(
            resolve_call("super::helper", "ff_agent::deep::inner::run", &fp),
            "ff_agent::deep::helper"
        );
    }

    #[test]
    fn std_call_kept_qualified() {
        let fp = fp_with("ff_agent::x", "ff_agent", &[]);
        assert_eq!(
            resolve_call("std::cmp::max", "ff_agent::x::f", &fp),
            "std::cmp::max"
        );
    }

    #[test]
    fn module_for_file_derives_crate_and_module() {
        // Synthetic check of the path math (no FS access for the module math).
        let (crate_name, module) =
            module_for_file("/nonexistent/crates/ff-agent/src/model_runtime.rs");
        // No Cargo.toml on disk -> crate falls back, but the path math still runs.
        assert!(module.ends_with("model_runtime") || module == crate_name);
        let _ = crate_name;
    }

    #[test]
    fn alias_map_brace_self_binding() {
        // `use crate::model_runtime::{self, load_model};` should bind both the
        // module alias `model_runtime` and the fn alias `load_model`.
        let mut fp = fp_with("ff_agent::autoscaler", "ff_agent", &[]);
        register_use("ff_agent::model_runtime", "model_runtime", &mut fp);
        register_use("ff_agent::model_runtime::load_model", "load_model", &mut fp);
        assert_eq!(
            fp.alias_map.get("model_runtime").unwrap(),
            "ff_agent::model_runtime"
        );
        let got = resolve_call("load_model", "ff_agent::autoscaler::do_load", &fp);
        // bare load_model resolves to the caller module (NOT the alias) — both are
        // valid call forms; this asserts the bare path is module-relative.
        assert_eq!(got, "ff_agent::autoscaler::load_model");
    }

    #[test]
    fn parse_extracts_function_and_call() {
        let src = "pub fn alpha() { beta(); }\nfn beta() {}\n";
        let fp = parse_rust_file("/x/crates/demo/src/lib.rs", src).unwrap();
        let fns: Vec<&str> = fp
            .symbols
            .iter()
            .filter(|s| s.node_type == "code:function")
            .map(|s| s.qualified_name.as_str())
            .collect();
        assert!(fns.iter().any(|q| q.ends_with("::alpha")));
        assert!(fns.iter().any(|q| q.ends_with("::beta")));
        assert!(fp.calls.iter().any(|c| c.raw_path == "beta"));
    }

    #[test]
    fn ts_parse_extracts_class_function_import_call() {
        let src = r#"
import { helper, fmt as format } from './util';
import * as svc from './svc';

export function alpha() {
  helper();
  svc.run();
  beta();
}

function beta() {}

export class Greeter {
  greet() {
    this.salute();
    format();
    return new Greeter();
  }
  salute() {}
}
"#;
        // No package.json on disk -> pkg ident falls back to "pkg" and modules
        // collapse to the bare file stem (main / util / svc) — deterministic.
        let fp = parse_typescript_file("/nonexistent/demo/src/main.ts", src).unwrap();
        assert_eq!(fp.lang, Lang::TypeScript);
        let names: Vec<(&str, &str)> = fp
            .symbols
            .iter()
            .map(|s| (s.node_type, s.qualified_name.as_str()))
            .collect();
        assert!(names.contains(&("code:function", "pkg::main::alpha")));
        assert!(names.contains(&("code:function", "pkg::main::beta")));
        assert!(names.contains(&("code:class", "pkg::main::Greeter")));
        assert!(names.contains(&("code:function", "pkg::main::Greeter::greet")));
        assert!(names.contains(&("code:function", "pkg::main::Greeter::salute")));
        // methods hang off the class (contains edge source = parent symbol).
        let class_idx = fp
            .symbols
            .iter()
            .position(|s| s.qualified_name == "pkg::main::Greeter")
            .unwrap();
        let greet = fp
            .symbols
            .iter()
            .find(|s| s.qualified_name == "pkg::main::Greeter::greet")
            .unwrap();
        assert_eq!(greet.parent, Some(class_idx));
        // imports + aliases (relative './util' resolves to the util module).
        assert!(fp.use_targets.iter().any(|t| t == "pkg::util::helper"));
        assert_eq!(fp.alias_map.get("format").unwrap(), "pkg::util::fmt");
        assert_eq!(fp.alias_map.get("svc").unwrap(), "pkg::svc");
        // raw call shapes
        let raws: Vec<&str> = fp.calls.iter().map(|c| c.raw_path.as_str()).collect();
        assert!(raws.contains(&"helper"));
        assert!(raws.contains(&"svc::run"));
        assert!(raws.contains(&"this::salute"));
        assert!(raws.contains(&"beta"));
        assert!(raws.contains(&"Greeter::constructor"));
        // resolution: imported bare call -> defining module (alias-first);
        // namespace member -> alias-expanded; bare local -> caller module;
        // this.m() -> enclosing class.
        assert_eq!(
            resolve_call("helper", "pkg::main::alpha", &fp),
            "pkg::util::helper"
        );
        assert_eq!(
            resolve_call("svc::run", "pkg::main::alpha", &fp),
            "pkg::svc::run"
        );
        assert_eq!(
            resolve_call("beta", "pkg::main::alpha", &fp),
            "pkg::main::beta"
        );
        assert_eq!(
            resolve_call("this::salute", "pkg::main::Greeter::greet", &fp),
            "pkg::main::Greeter::salute"
        );
        assert_eq!(
            resolve_call("Greeter::constructor", "pkg::main::Greeter::greet", &fp),
            "pkg::main::Greeter::constructor"
        );
    }

    #[test]
    fn java_parse_extracts_package_class_method_import_call() {
        let src = r#"
package com.acme.auth;

import com.acme.util.Strings;
import static com.acme.util.Asserts.check;
import java.util.*;

public class AuthService {
    public String login(String user) {
        check(user);
        Strings.trim(user);
        this.validate(user);
        audit(user);
        Session s = new Session(user);
        return user;
    }

    void validate(String u) {}

    static class Tokens {
        void mint() {}
    }
}

class Session {
    Session(String u) {}
}
"#;
        let fp = parse_java_file("/nonexistent/AuthService.java", src).unwrap();
        assert_eq!(fp.lang, Lang::Java);
        assert_eq!(fp.module, "com::acme::auth");
        let names: Vec<(&str, &str)> = fp
            .symbols
            .iter()
            .map(|s| (s.node_type, s.qualified_name.as_str()))
            .collect();
        assert!(names.contains(&("code:class", "com::acme::auth::AuthService")));
        assert!(names.contains(&("code:function", "com::acme::auth::AuthService::login")));
        assert!(names.contains(&("code:function", "com::acme::auth::AuthService::validate")));
        // nested type + its method qualify under the outer class
        assert!(names.contains(&("code:class", "com::acme::auth::AuthService::Tokens")));
        assert!(names.contains(&(
            "code:function",
            "com::acme::auth::AuthService::Tokens::mint"
        )));
        // top-level sibling class + constructor
        assert!(names.contains(&("code:class", "com::acme::auth::Session")));
        assert!(names.contains(&("code:function", "com::acme::auth::Session::Session")));
        // imports: plain, static, wildcard
        assert!(
            fp.use_targets
                .iter()
                .any(|t| t == "com::acme::util::Strings")
        );
        assert!(
            fp.use_targets
                .iter()
                .any(|t| t == "com::acme::util::Asserts::check")
        );
        assert!(fp.use_targets.iter().any(|t| t == "java::util"));
        assert_eq!(
            fp.alias_map.get("Strings").unwrap(),
            "com::acme::util::Strings"
        );
        // raw call shapes
        let raws: Vec<&str> = fp.calls.iter().map(|c| c.raw_path.as_str()).collect();
        assert!(raws.contains(&"check"));
        assert!(raws.contains(&"Strings::trim"));
        assert!(raws.contains(&"this::validate"));
        assert!(raws.contains(&"audit"));
        assert!(raws.contains(&"Session::Session"));
        // resolution
        let login = "com::acme::auth::AuthService::login";
        // static import wins for bare calls
        assert_eq!(
            resolve_call("check", login, &fp),
            "com::acme::util::Asserts::check"
        );
        // bare non-imported call -> enclosing class
        assert_eq!(
            resolve_call("audit", login, &fp),
            "com::acme::auth::AuthService::audit"
        );
        // imported class static call
        assert_eq!(
            resolve_call("Strings::trim", login, &fp),
            "com::acme::util::Strings::trim"
        );
        // this.m() -> enclosing class (matches the real code:function node)
        assert_eq!(
            resolve_call("this::validate", login, &fp),
            "com::acme::auth::AuthService::validate"
        );
        // same-package constructor
        assert_eq!(
            resolve_call("Session::Session", login, &fp),
            "com::acme::auth::Session::Session"
        );
        // unknown lower-case receiver stays as written (extern)
        assert_eq!(resolve_call("userRepo::save", login, &fp), "userRepo::save");
    }

    #[test]
    fn python_parse_extracts_class_def_import_call() {
        let src = r#"
from acme.util import check, Strings as Str
from .helpers import audit
import os.path

GREETING = "hi"


class AuthService:
    def login(self, user):
        check(user)
        Str.trim(user)
        self.validate(user)
        audit(user)
        s = Session(user)
        os.path.join("a", "b")
        return user

    def validate(self, u):
        pass


class Session:
    def __init__(self, u):
        pass
"#;
        // No __init__.py on disk -> module collapses to the bare file stem
        // ("auth_service") — deterministic, like the TS/Java fake-path tests.
        let fp = parse_python_file("/nonexistent/auth_service.py", src).unwrap();
        assert_eq!(fp.lang, Lang::Python);
        assert_eq!(fp.module, "auth_service");
        let names: Vec<(&str, &str)> = fp
            .symbols
            .iter()
            .map(|s| (s.node_type, s.qualified_name.as_str()))
            .collect();
        assert!(names.contains(&("code:class", "auth_service::AuthService")));
        assert!(names.contains(&("code:function", "auth_service::AuthService::login")));
        assert!(names.contains(&("code:function", "auth_service::AuthService::validate")));
        assert!(names.contains(&("code:class", "auth_service::Session")));
        assert!(names.contains(&("code:function", "auth_service::Session::__init__")));
        // methods hang off the class (contains edge source = parent symbol).
        let class_idx = fp
            .symbols
            .iter()
            .position(|s| s.qualified_name == "auth_service::AuthService")
            .unwrap();
        let login_sym = fp
            .symbols
            .iter()
            .find(|s| s.qualified_name == "auth_service::AuthService::login")
            .unwrap();
        assert_eq!(login_sym.parent, Some(class_idx));
        // imports + aliases: `from m import x`, `x as y`, relative, plain dotted.
        assert!(fp.use_targets.iter().any(|t| t == "acme::util::check"));
        assert_eq!(fp.alias_map.get("check").unwrap(), "acme::util::check");
        assert_eq!(fp.alias_map.get("Str").unwrap(), "acme::util::Strings");
        assert_eq!(fp.alias_map.get("audit").unwrap(), "helpers::audit");
        assert!(fp.use_targets.iter().any(|t| t == "os::path"));
        // raw call shapes
        let raws: Vec<&str> = fp.calls.iter().map(|c| c.raw_path.as_str()).collect();
        assert!(raws.contains(&"check"));
        assert!(raws.contains(&"Str::trim"));
        assert!(raws.contains(&"self::validate"));
        assert!(raws.contains(&"audit"));
        assert!(raws.contains(&"Session"));
        assert!(raws.contains(&"os::path::join"));
        // resolution
        let login = "auth_service::AuthService::login";
        // imported bare call binds through the alias map (the dominant Python form)
        assert_eq!(resolve_call("check", login, &fp), "acme::util::check");
        // aliased import receiver expands
        assert_eq!(
            resolve_call("Str::trim", login, &fp),
            "acme::util::Strings::trim"
        );
        // self.m() -> the enclosing class (matches the real code:function node)
        assert_eq!(
            resolve_call("self::validate", login, &fp),
            "auth_service::AuthService::validate"
        );
        // relative `from .helpers import audit` binds to its package leaf
        assert_eq!(resolve_call("audit", login, &fp), "helpers::audit");
        // plain `import os.path` stays an already-qualified extern receiver
        assert_eq!(resolve_call("os::path::join", login, &fp), "os::path::join");
    }
}
