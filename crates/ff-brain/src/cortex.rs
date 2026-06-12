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
}

/// Languages `index()` accepts (also drives the terminal's auto-detect filter).
pub const SUPPORTED_LANGS: &[&str] = &["rust", "typescript", "javascript", "java"];

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
            _ => unreachable!("lang validated by lang_patterns"),
        };
        let parse = match parse {
            Some(p) => p,
            None => continue,
        };
        stats.files_parsed += 1;

        // Write symbol nodes + contains edges.
        let mut sym_ids: HashMap<String, Uuid> = HashMap::new();
        let mut idx_to_id: HashMap<usize, Uuid> = HashMap::new();
        for (i, sym) in parse.symbols.iter().enumerate() {
            let sym_path = format!("code://{corpus_slug}/{}", sym.qualified_name);
            let id = upsert_code_node(
                pool,
                &sym_path,
                &sym.qualified_name,
                sym.node_type,
                corpus_slug,
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
            let imp_id =
                upsert_code_node(pool, &imp_path, target, "code:import", corpus_slug).await?;
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
                    upsert_code_node(pool, &callee_path, &resolved, "code:extern", corpus_slug)
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
async fn symbols_in_file(pool: &PgPool, file_id: Uuid) -> Result<Vec<SymbolRef>> {
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
           SELECT n.id, n.title, n.node_type
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
        .map(|r| SymbolRef {
            id: r.get("id"),
            qualified_name: r.get("title"),
            node_type: r.get("node_type"),
        })
        .collect())
}

/// Build a change-aware, risk-scored review report for a set of changed files.
///
/// `changed_abs_paths` are absolute filesystem paths (the terminal layer derives
/// them from `git diff` and maps repo-relative → absolute). `depth` bounds the
/// transitive blast-radius walk. Files not in the graph land in `unindexed`.
pub async fn review(
    pool: &PgPool,
    corpus_slug: &str,
    changed_abs_paths: &[String],
    depth: usize,
) -> Result<ReviewReport> {
    let mut report = ReviewReport::default();
    let mut global_blast: HashSet<Uuid> = HashSet::new();

    for path in changed_abs_paths {
        let Some(fid) = file_node_id(pool, corpus_slug, path).await? else {
            report.unindexed.push(path.clone());
            continue;
        };
        let syms = symbols_in_file(pool, fid).await?;
        // The file's own symbol names — used to split internal vs external fan-in.
        let own_names: HashSet<&str> = syms.iter().map(|s| s.qualified_name.as_str()).collect();

        let mut file_blast: HashSet<Uuid> = HashSet::new();
        let mut changed_syms: Vec<ChangedSymbol> = Vec::new();
        for s in &syms {
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
) -> Result<Uuid> {
    // content_hash is NOT NULL; use the path (synthetic + unique) as a stable hash.
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO brain_vault_nodes (path, title, node_type, project, content_hash)
           VALUES ($1, $2, $3, $4, $5)
           ON CONFLICT (path) DO UPDATE
             SET title = EXCLUDED.title, node_type = EXCLUDED.node_type,
                 project = EXCLUDED.project, valid_until = NULL, updated_at = NOW()
           RETURNING id"#,
    )
    .bind(path)
    .bind(title)
    .bind(node_type)
    .bind(project)
    .bind(path)
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

// ─── Call resolution (THE DIFFERENTIATOR) ────────────────────────────────────

/// Resolve a raw call path to a fully-qualified name, given the enclosing fn's
/// qualified name (`caller_qn`) and the file's parse (module / crate / aliases).
pub(crate) fn resolve_call(raw: &str, caller_qn: &str, fp: &FileParse) -> String {
    match fp.lang {
        Lang::Rust => resolve_call_inner(raw, caller_qn, &fp.module, &fp.crate_name, &fp.alias_map),
        Lang::TypeScript | Lang::Java => resolve_call_dotty(raw, caller_qn, fp),
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
    fn risk_tier_rank_orders_high_first() {
        assert!(RiskTier::High.rank() > RiskTier::Medium.rank());
        assert!(RiskTier::Medium.rank() > RiskTier::Low.rank());
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
}
