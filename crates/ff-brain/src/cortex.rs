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

#[path = "cortex/extractors/api.rs"]
pub mod api;
#[path = "cortex/audit.rs"]
pub mod audit;
#[path = "cortex/extractors/code_symbols.rs"]
pub mod code_symbols;
#[path = "cortex/extractors/config.rs"]
pub mod config;
#[path = "cortex/extractors/dataflow.rs"]
pub mod dataflow;
#[path = "cortex/extractors/db_schema.rs"]
pub mod db_schema;
#[path = "cortex/extractors/deps.rs"]
pub mod deps;
#[path = "cortex/extractors/events.rs"]
pub mod events;
#[path = "cortex/ingest_biz.rs"]
pub mod ingest_biz;
#[path = "cortex/ingest_decisions.rs"]
pub mod ingest_decisions;
#[path = "cortex/ingest_fleet.rs"]
pub mod ingest_fleet;
#[path = "cortex/ingest_people.rs"]
pub mod ingest_people;
#[path = "cortex/ingest_pm.rs"]
pub mod ingest_pm;
#[path = "cortex/maps_to.rs"]
pub mod maps_to;
#[path = "cortex/extractors/observ.rs"]
pub mod observ;
#[path = "cortex/extractors/owners.rs"]
pub mod owners;
#[path = "cortex/extractors/product.rs"]
pub mod product;
#[path = "cortex/extractors/routes.rs"]
pub mod routes;
#[path = "cortex/extractors/security.rs"]
pub mod security;
#[path = "cortex/spi.rs"]
pub mod spi;
#[path = "cortex/extractors/types.rs"]
pub mod types;

/// Summary of a Cortex indexing run.
#[derive(Debug, Default, Clone)]
pub struct CortexStats {
    pub files_parsed: usize,
    pub symbols: usize,
    pub calls_total: usize,
    pub calls_resolved: usize,
    /// Subset of `calls_resolved` that resolved to a real internal symbol ONLY via
    /// a heuristic redirect (glob / impl-method / `Self::` / facade), not the
    /// primary resolver — the INFERRED tier (confidence 0.6). EXTRACTED =
    /// `calls_resolved - calls_inferred`.
    pub calls_inferred: usize,
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
    /// Glob-import prefixes from `use <prefix>::*` (Rust). `super`/`self` are kept
    /// relative and resolved against the call site; everything else is already a
    /// fully-qualified module path. Used by [`resolve_glob_call`] to redirect a
    /// bare call brought into scope by a glob (the `mod tests { use super::*; }`
    /// pattern) to the real symbol instead of fabricating a `code:extern`.
    glob_imports: Vec<String>,
    /// `pub use` re-exports: (facade_qn, real_target_qn). A facade crate root
    /// (`pub use crate::migrations::run_postgres_migrations;` in `ff_db`'s lib.rs) makes
    /// `ff_db::run_postgres_migrations` an alias for `ff_db::migrations::run_postgres_migrations`.
    /// Calls through the facade path otherwise resolve to a `code:extern`. Both
    /// halves are stored absolute; the corpus-wide map (built in pass 1) drives a
    /// redirect-only pass at resolve time. Rust only.
    reexports: Vec<(String, String)>,
    /// Glob `pub use` re-exports: (facade_base_module, real_target_module). A
    /// `pub use leader_state::*;` in `ff_db`'s lib.rs re-exports EVERY item of
    /// `ff_db::leader_state` at the crate root, so `ff_db::pg_get_current_leader`
    /// is an alias for `ff_db::leader_state::pg_get_current_leader`. Unlike a named
    /// `pub use`, the leaf set isn't known at parse time, so it can't go in
    /// `reexports`; the resolve-time pass checks `<target>::<leaf>` against the
    /// internal fn set. Rust only.
    glob_reexports: Vec<(String, String)>,
    /// Wildcard import bases for the dotty languages (Java/TS/Python), the
    /// non-Rust analogue of [`glob_imports`]. A Java `import static a.b.C.*;`
    /// static wildcard or `import a.b.*;` package wildcard brings members into
    /// scope WITHOUT binding an alias leaf, so a bare call to one of those
    /// members otherwise fabricates `<caller_module>::<leaf>` — an extern under
    /// the CALLER's path instead of the real definition. Each entry is the
    /// fully-qualified base the wildcard expands under (`a::b::C` for the static
    /// wildcard, `a::b` for the package wildcard). Used by
    /// [`resolve_wildcard_call`] to redirect a bare leaf to `<base>::<leaf>` when
    /// exactly one base names a known internal fn (redirect-only, never
    /// fabricates). Rust globs use [`glob_imports`] instead.
    wildcard_imports: Vec<String>,
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
    let Some(lock) = try_begin_cortex_generation(pool, corpus_slug).await? else {
        tracing::info!("another node is indexing {corpus_slug}, skipping");
        return Ok(CortexStats::default());
    };
    let generation = lock.generation;
    let result = async {
        wipe_code_nodes(pool, corpus_slug).await?;
        clear_file_index(pool, corpus_slug).await?; // reset the incremental ledger
        clear_reexports(pool, corpus_slug).await?; // reset the facade ledger
        let stats = index_one_generation(pool, corpus_slug, lang, generation).await?;
        run_pluggable_extractors(pool, corpus_slug, generation).await?;
        Ok(stats)
    }
    .await;
    finish_cortex_generation(pool, corpus_slug, lock, result).await
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
    let Some(lock) = try_begin_cortex_generation(pool, corpus_slug).await? else {
        tracing::info!("another node is indexing {corpus_slug}, skipping");
        return Ok(Vec::new());
    };
    let generation = lock.generation;
    let result = async {
        wipe_code_nodes(pool, corpus_slug).await?;
        // A full reindex re-stamps every file's hash from scratch — drop the prior
        // ledger so removed files don't linger as "already indexed" rows. index_one
        // records each file's current hash as it extracts.
        clear_file_index(pool, corpus_slug).await?;
        clear_reexports(pool, corpus_slug).await?;
        let mut out = Vec::with_capacity(langs.len());
        for l in langs {
            let stats = index_one_generation(pool, corpus_slug, l, generation).await?;
            out.push((l.clone(), stats));
        }
        run_pluggable_extractors(pool, corpus_slug, generation).await?;
        Ok(out)
    }
    .await;
    finish_cortex_generation(pool, corpus_slug, lock, result).await
}

/// Summary of an incremental reindex: which files were touched + per-language
/// extraction stats for the changed subset.
#[derive(Debug, Default, Clone)]
pub struct IncrementalReport {
    pub files_changed: usize,
    pub files_unchanged: usize,
    pub files_deleted: usize,
    /// Unreferenced `code:extern`/`code:import` placeholder nodes garbage-collected
    /// this run (the dead-placeholder accrual that previously needed a full reindex).
    pub placeholders_gced: usize,
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
/// `code:extern`/`code:import` placeholder nodes that this run's edge churn
/// leaves unreferenced are garbage-collected at the end (`gc_orphan_placeholders`),
/// so incremental is self-sufficient — no periodic full reindex is needed to keep
/// the graph clean (the commit hook only ever runs `--incremental`). First run on a
/// corpus with no ledger treats every file as changed — equivalent to a full
/// reindex but without the upfront global wipe.
pub async fn index_langs_incremental(
    pool: &PgPool,
    corpus_slug: &str,
    langs: &[String],
) -> Result<IncrementalReport> {
    for l in langs {
        lang_patterns(l)?;
    }
    let Some(lock) = try_begin_cortex_generation(pool, corpus_slug).await? else {
        tracing::info!("another node is indexing {corpus_slug}, skipping");
        return Ok(IncrementalReport::default());
    };
    let generation = lock.generation;
    let result = async {
        let corpus_id: Uuid = sqlx::query_scalar("SELECT id FROM brain_corpora WHERE slug = $1")
            .bind(corpus_slug)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no corpus with slug '{corpus_slug}'"))?;

        // What Cortex last indexed: file_path -> indexed_hash.
        let tracked: HashMap<String, String> = sqlx::query(
            "SELECT file_path, indexed_hash FROM cortex_file_index WHERE corpus_slug = $1",
        )
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

        // Drop symbols of removed files first (so their fns leave internal_fns). Gather
        // every deleted file's symbols, then remove them in one pass (below) so a call
        // BETWEEN two deleted files is treated as removed at both ends rather than
        // spuriously demoting one to an extern.
        let mut deleted_syms: Vec<Uuid> = Vec::new();
        for path in &deleted {
            if let Some(fid) = lookup_file_node(pool, corpus_slug, path).await? {
                deleted_syms.extend(file_symbol_ids(pool, &[fid]).await?);
                // The file node's own outgoing import edges go with the file.
                sqlx::query(
                    "DELETE FROM brain_vault_edges WHERE src_id = $1 AND edge_type = 'imports'",
                )
                .bind(fid)
                .execute(pool)
                .await?;
            }
            sqlx::query("DELETE FROM cortex_file_index WHERE corpus_slug = $1 AND file_path = $2")
                .bind(corpus_slug)
                .bind(path)
                .execute(pool)
                .await?;
            sqlx::query("DELETE FROM cortex_reexports WHERE corpus_slug = $1 AND file_path = $2")
                .bind(corpus_slug)
                .bind(path)
                .execute(pool)
                .await?;
            report.files_deleted += 1;
        }
        // Demote-or-delete: a deleted file's symbol still called by a SURVIVING file
        // must become a `code:extern` placeholder, not vanish — a full reindex would
        // re-create exactly that placeholder for the caller's now-unresolved call.
        remove_code_symbols(pool, &deleted_syms).await?;

        report.files_changed = changed.len();
        if changed.is_empty() {
            // No re-extraction, but deletions can orphan a removed file's import/
            // extern placeholders — sweep them. (A run that touched nothing at all
            // can't have orphaned anything, so skip the query then.)
            if report.files_deleted > 0 {
                report.placeholders_gced = gc_orphan_placeholders(pool, corpus_slug).await?;
            }
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
        // internal_types gates the inherent-impl method redirect. It only needs to be
        // permissive-but-correct: a stale type entry merely lets a redirect fire whose
        // target must STILL be a real internal fn, so we load the whole corpus and let
        // pass 1 re-add changed files' current types — no need to prune old ones.
        let mut internal_types = load_internal_types(pool, corpus_slug).await?;

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
            let stats = extract_files(
                pool,
                corpus_id,
                corpus_slug,
                l,
                &rows,
                &mut internal_fns,
                &mut internal_types,
                generation,
            )
            .await?;
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
        // A removed symbol still called by an UNCHANGED file must demote to an extern,
        // not vanish (see remove_code_symbols) — else hard-deleting it cascade-drops
        // the unchanged caller's `calls` edge and the call silently disappears.
        remove_code_symbols(pool, &removed).await?;

        // Re-extraction may have dropped the last `imports`/`calls` edge into an
        // extern/import placeholder (a removed import, a renamed/deleted callee).
        // GC those zero-in-degree placeholders so incremental matches a full reindex.
        report.placeholders_gced = gc_orphan_placeholders(pool, corpus_slug).await?;
        // Run the pluggable dimension extractors (db/events/config/…) too — they
        // re-derive from the corpus roots, so they run on incremental reindex just
        // like the full path. Without this the registry only fires on a full
        // reindex, and `ff cortex index` (incremental) would never write db:* etc.
        run_pluggable_extractors(pool, corpus_slug, generation).await?;
        Ok(report)
    }
    .await;
    finish_cortex_generation(pool, corpus_slug, lock, result).await
}

/// Garbage-collect unreferenced placeholder nodes (`code:extern`/`code:import`)
/// in a corpus. A placeholder exists only because some file imports or calls the
/// external/unresolved symbol it names; the incoming `imports`/`calls` edge IS its
/// reason to exist. Incremental reindex (unlike a full reindex, which wipes and
/// rebuilds) keeps shared placeholders across file churn, so when the last
/// referencing file changes or is removed a placeholder can be left with zero
/// incoming edges — dead weight that pollutes `find`/`outline` and bloats the
/// graph. This deletes exactly those zero-in-degree placeholders, which is what a
/// full reindex would (a full reindex re-creates a placeholder only when a file
/// references it, i.e. only with an incoming edge). Scoped strictly to the two
/// placeholder node_types, so real `code:function`/`code:struct`/… symbols — which
/// can legitimately have no incoming `calls` edge (entrypoints, dead-but-defined) —
/// are never touched. Returns the number of nodes deleted.
async fn gc_orphan_placeholders(pool: &PgPool, corpus_slug: &str) -> Result<usize> {
    let res = sqlx::query(
        r#"DELETE FROM brain_vault_nodes
            WHERE project = $1
              AND node_type IN ('code:extern', 'code:import')
              AND NOT EXISTS (
                  SELECT 1 FROM brain_vault_edges e
                   WHERE e.dst_id = brain_vault_nodes.id
              )"#,
    )
    .bind(corpus_slug)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() as usize)
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

/// Run every registered pluggable extractor (the universal-graph dimensions:
/// db, events, config, security, …) for this corpus, persisting their Facts
/// stamped with the in-progress `generation`. Code symbols are written by the
/// in-tree per-language pass; this is the single plug point new dimensions append
/// to via [`spi::registry`], so a fan-out PR is one new module + one registry line.
async fn run_pluggable_extractors(pool: &PgPool, corpus_slug: &str, generation: i64) -> Result<()> {
    let roots = corpus_root_paths(pool, corpus_slug).await?;
    let ctx = spi::ExtractCtx {
        pool,
        corpus_slug,
        roots,
        generation,
        incremental: false,
    };
    for extractor in spi::registry() {
        if !extractor.applies(&ctx) {
            continue;
        }
        let facts = extractor.extract(&ctx).await?;
        if !facts.is_empty() {
            spi::write_facts(pool, corpus_slug, generation, &facts).await?;
        }
    }
    Ok(())
}

/// On-disk source roots for a corpus (its `brain_sources.root_path` rows) — what
/// non-code extractors walk for `.sql`/config/etc. files.
async fn corpus_root_paths(pool: &PgPool, corpus_slug: &str) -> Result<Vec<std::path::PathBuf>> {
    let rows: Vec<String> = sqlx::query_scalar(
        r#"SELECT bs.root_path
             FROM brain_sources bs
             JOIN brain_corpora bc ON bc.id = bs.corpus_id
            WHERE bc.slug = $1"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(std::path::PathBuf::from).collect())
}

struct CortexGenerationLock {
    generation: i64,
    /// Dedicated connection that holds the session-level advisory lock for the
    /// WHOLE reindex. Kept checked out of the pool so it is NOT idle-reaped
    /// mid-pass — the pool's `idle_timeout` (60s) would otherwise drop the
    /// lock-holding connection partway through a large (multi-minute) reindex,
    /// surfacing as "connection closed / got 0 bytes at EOF" and a truncated graph.
    conn: sqlx::pool::PoolConnection<sqlx::Postgres>,
}

async fn try_begin_cortex_generation(
    pool: &PgPool,
    corpus_slug: &str,
) -> Result<Option<CortexGenerationLock>> {
    let lock_key = format!("cortex:{corpus_slug}");
    // Hold the advisory lock on a DEDICATED connection (checked out of the pool)
    // for the whole reindex — pg_try_advisory_lock is session-scoped, so the lock
    // lives exactly as long as this connection stays checked out. See the
    // CortexGenerationLock doc for why this matters (idle-reaping mid-reindex).
    let mut conn = pool.acquire().await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
        .bind(&lock_key)
        .fetch_one(&mut *conn)
        .await?;
    if !acquired {
        return Ok(None);
    }

    let node = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string());
    let generation: i64 = sqlx::query_scalar(
        r#"INSERT INTO cortex_generations
               (project, current_generation, indexing_node, indexing_started, updated_at)
           VALUES ($1, 0, $2, NOW(), NOW())
           ON CONFLICT (project) DO UPDATE
             SET indexing_node = EXCLUDED.indexing_node,
                 indexing_started = EXCLUDED.indexing_started,
                 updated_at = NOW()
           RETURNING current_generation + 1"#,
    )
    .bind(corpus_slug)
    .bind(node)
    .fetch_one(&mut *conn)
    .await?;

    Ok(Some(CortexGenerationLock { generation, conn }))
}

async fn finish_cortex_generation<T>(
    _pool: &PgPool,
    corpus_slug: &str,
    mut lock: CortexGenerationLock,
    result: Result<T>,
) -> Result<T> {
    // Run the swap + unlock on the SAME dedicated connection that took the lock —
    // pg_advisory_unlock must run in the locking session, and this connection is
    // guaranteed alive (it was held checked-out for the whole reindex). The reindex
    // work itself used the pool.
    let lock_key = format!("cortex:{corpus_slug}");
    match result {
        Ok(value) => {
            // Carry every still-live node forward to the new generation BEFORE the
            // swap. The read path (find_symbols, callers/callees/impact, …) filters
            // `generation IN (0, current_generation)`, so a node is only visible at
            // the current generation. An INCREMENTAL reindex writes just the changed
            // symbols at the new generation and leaves unchanged ones at whatever
            // generation they were last written — which, once current_generation
            // advances, falls BELOW it and vanishes from every query (the forge-fleet
            // corpus had 12k live symbols stranded at gen 72 while current_generation
            // had crept to 115, so `ff cortex find/explain` returned nothing).
            // Promoting all live (`valid_until IS NULL`) nodes to the new generation
            // keeps the whole live graph visible and self-heals any prior scatter.
            // Superseded/deleted nodes carry `valid_until` and are correctly skipped.
            sqlx::query(
                r#"UPDATE brain_vault_nodes
                      SET generation = $2
                    WHERE project = $1
                      AND valid_until IS NULL
                      AND COALESCE(generation, 0) <> $2"#,
            )
            .bind(corpus_slug)
            .bind(lock.generation)
            .execute(&mut *lock.conn)
            .await?;

            sqlx::query(
                r#"UPDATE cortex_generations
                      SET current_generation = $2,
                          last_swapped = NOW(),
                          indexing_node = NULL,
                          updated_at = NOW()
                    WHERE project = $1"#,
            )
            .bind(corpus_slug)
            .bind(lock.generation)
            .execute(&mut *lock.conn)
            .await?;
            let _: bool = sqlx::query_scalar("SELECT pg_advisory_unlock(hashtext($1))")
                .bind(&lock_key)
                .fetch_one(&mut *lock.conn)
                .await?;
            Ok(value)
        }
        Err(err) => {
            let _ = sqlx::query(
                r#"UPDATE cortex_generations
                      SET indexing_node = NULL,
                          updated_at = NOW()
                    WHERE project = $1"#,
            )
            .bind(corpus_slug)
            .execute(&mut *lock.conn)
            .await;
            let _ = sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock(hashtext($1))")
                .bind(&lock_key)
                .fetch_one(&mut *lock.conn)
                .await;
            Err(err)
        }
    }
}

async fn current_generation(pool: &PgPool, corpus_slug: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT current_generation FROM cortex_generations WHERE project = $1")
            .bind(corpus_slug)
            .fetch_optional(pool)
            .await?
            .unwrap_or(0),
    )
}

#[allow(dead_code)]
async fn sweep_old_generations(pool: &PgPool, corpus_slug: &str) -> Result<()> {
    let generation = current_generation(pool, corpus_slug).await?;
    sqlx::query(
        r#"DELETE FROM brain_vault_edges e
            USING brain_vault_nodes s
           WHERE e.src_id = s.id
             AND s.project = $1
             AND e.generation IS NOT NULL
             AND e.generation < $2"#,
    )
    .bind(corpus_slug)
    .bind(generation - 1)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"DELETE FROM brain_vault_nodes
            WHERE project = $1
              AND generation IS NOT NULL
              AND generation < $2"#,
    )
    .bind(corpus_slug)
    .bind(generation - 1)
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

/// Drop the whole re-export ledger for a corpus (full reindex re-records it).
async fn clear_reexports(pool: &PgPool, corpus_slug: &str) -> Result<()> {
    sqlx::query("DELETE FROM cortex_reexports WHERE corpus_slug = $1")
        .bind(corpus_slug)
        .execute(pool)
        .await?;
    Ok(())
}

/// Replace one file's re-export rows with its current `pub use` set. Called in
/// pass 1 so the corpus-wide facade map ([`load_reexports`]) reflects the changed
/// file's CURRENT re-exports — a removed `pub use` drops out (delete-then-insert),
/// an added one appears. No-op-cheap for the common file with no `pub use`.
async fn record_file_reexports(
    pool: &PgPool,
    corpus_slug: &str,
    file_path: &str,
    parse: &FileParse,
) -> Result<()> {
    sqlx::query("DELETE FROM cortex_reexports WHERE corpus_slug = $1 AND file_path = $2")
        .bind(corpus_slug)
        .bind(file_path)
        .execute(pool)
        .await?;
    for (facade, target) in &parse.reexports {
        sqlx::query(
            r#"INSERT INTO cortex_reexports (corpus_slug, file_path, kind, facade, target)
               VALUES ($1, $2, 'named', $3, $4)
               ON CONFLICT DO NOTHING"#,
        )
        .bind(corpus_slug)
        .bind(file_path)
        .bind(facade)
        .bind(target)
        .execute(pool)
        .await?;
    }
    for (base, target) in &parse.glob_reexports {
        sqlx::query(
            r#"INSERT INTO cortex_reexports (corpus_slug, file_path, kind, facade, target)
               VALUES ($1, $2, 'glob', $3, $4)
               ON CONFLICT DO NOTHING"#,
        )
        .bind(corpus_slug)
        .bind(file_path)
        .bind(base)
        .bind(target)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Load the corpus-wide `pub use` facade map from the ledger: named re-exports
/// (`facade -> target`) and glob re-exports (`base_module -> [target_module, …]`).
/// Whole-corpus, so an incremental reindex resolves a facade call in a changed
/// file even when the owning `lib.rs` wasn't touched this run — the same role
/// [`load_internal_fns`] plays for functions.
async fn load_reexports(
    pool: &PgPool,
    corpus_slug: &str,
) -> Result<(HashMap<String, String>, HashMap<String, Vec<String>>)> {
    let rows =
        sqlx::query("SELECT kind, facade, target FROM cortex_reexports WHERE corpus_slug = $1")
            .bind(corpus_slug)
            .fetch_all(pool)
            .await?;
    let mut named: HashMap<String, String> = HashMap::new();
    let mut glob: HashMap<String, Vec<String>> = HashMap::new();
    for r in rows {
        let kind: String = r.get("kind");
        let facade: String = r.get("facade");
        let target: String = r.get("target");
        if kind == "glob" {
            glob.entry(facade).or_default().push(target);
        } else {
            named.entry(facade).or_insert(target);
        }
    }
    Ok((named, glob))
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

/// Split a removed-symbol set into (demote, delete) given the subset that still
/// has a surviving incoming caller. `still_called` is the distinct list of removed
/// nodes that a surviving (non-removed) node still `calls` into; those are demoted
/// to externs, the rest are deleted outright. Pure set logic — DB work lives in
/// `remove_code_symbols`. Defensive: a `still_called` entry not in `removed` is
/// ignored (only nodes we were asked to remove can be demoted).
fn partition_demote_delete(removed: &[Uuid], still_called: &[Uuid]) -> (Vec<Uuid>, Vec<Uuid>) {
    let removed_set: HashSet<Uuid> = removed.iter().copied().collect();
    let demote: Vec<Uuid> = still_called
        .iter()
        .copied()
        .filter(|id| removed_set.contains(id))
        .collect();
    let demote_set: HashSet<Uuid> = demote.iter().copied().collect();
    let delete: Vec<Uuid> = removed
        .iter()
        .copied()
        .filter(|id| !demote_set.contains(id))
        .collect();
    (demote, delete)
}

/// Remove code-symbol nodes that no longer exist in source — a changed file's
/// dropped functions, or a deleted file's whole symbol subtree. A node that still
/// has an incoming `calls` edge from a SURVIVING caller (one not itself in `ids`)
/// is DEMOTED in place to a `code:extern` placeholder instead of deleted: a full
/// reindex re-creates exactly that placeholder for the caller's now-unresolved
/// call, so hard-deleting the node (which cascade-drops the caller's edge) would
/// silently lose the call and drift the incremental graph from a full reindex
/// (`callees`/`doctor` would under-report). Nodes are path-keyed, so the in-place
/// type flip keeps the same id and carries the surviving incoming edge with it.
/// Nodes with no surviving caller are deleted (their edges cascade — genuinely
/// gone); a demoted extern that later loses its last caller is swept by
/// `gc_orphan_placeholders`. Shared `code:import`/`code:extern` placeholders are
/// untouched here (they are reference-counted by that GC).
async fn remove_code_symbols(pool: &PgPool, ids: &[Uuid]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    // Removed nodes a surviving (non-removed) node still `calls` into.
    let still_called: Vec<Uuid> = sqlx::query_scalar(
        r#"SELECT DISTINCT e.dst_id
             FROM brain_vault_edges e
            WHERE e.edge_type = 'calls'
              AND e.dst_id = ANY($1)
              AND e.src_id <> ALL($1)"#,
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    let (demote, delete) = partition_demote_delete(ids, &still_called);
    if !demote.is_empty() {
        // In-place flip to an unresolved extern (same path → same node id → the
        // surviving incoming `calls` edge is preserved). Externs carry no source
        // span / embedding / community, so clear those to match a fresh extern.
        sqlx::query(
            r#"UPDATE brain_vault_nodes
                  SET node_type = 'code:extern', start_line = NULL, end_line = NULL,
                      embedding = NULL, code_community_id = NULL, updated_at = NOW()
                WHERE id = ANY($1)"#,
        )
        .bind(&demote)
        .execute(pool)
        .await?;
        // An extern never calls out — drop any outgoing edges the old symbol had
        // (a deleted file's symbols still carry theirs at this point).
        delete_outgoing_edges(pool, &demote).await?;
    }
    delete_nodes_by_id(pool, &delete).await?;
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

/// Every internal TYPE qualified-name (struct / enum / impl + Java/TS
/// class / interface) in the corpus, so the inherent-impl method redirect
/// ([`resolve_impl_method_call`]) and the Java constructor redirect
/// ([`resolve_java_ctor_call`]) can tell a real `module::Type` apart from a
/// genuine module path. Same role for types as [`load_internal_fns`] plays for
/// functions on an incremental reindex.
async fn load_internal_types(pool: &PgPool, corpus_slug: &str) -> Result<HashSet<String>> {
    let titles: Vec<String> = sqlx::query_scalar(
        "SELECT title FROM brain_vault_nodes
           WHERE project = $1
             AND node_type IN ('code:struct', 'code:enum', 'code:impl',
                               'code:class', 'code:interface')",
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
async fn index_one_generation(
    pool: &PgPool,
    corpus_slug: &str,
    lang: &str,
    generation: i64,
) -> Result<CortexStats> {
    // Resolve corpus id.
    let corpus_id: Uuid = sqlx::query_scalar("SELECT id FROM brain_corpora WHERE slug = $1")
        .bind(corpus_slug)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no corpus with slug '{corpus_slug}'"))?;

    let file_rows = fetch_file_rows(pool, corpus_slug, lang).await?;
    // Full per-language extraction starts with empty internal sets: the graph was
    // just wiped, so every internal fn/type comes from these files.
    let mut internal_fns: HashSet<String> = HashSet::new();
    let mut internal_types: HashSet<String> = HashSet::new();
    extract_files(
        pool,
        corpus_id,
        corpus_slug,
        lang,
        &file_rows,
        &mut internal_fns,
        &mut internal_types,
        generation,
    )
    .await
}

/// Two-pass extraction over a set of files: write symbol nodes + contains +
/// imports (pass 1, also populating `internal_fns` + `internal_types`), then
/// resolve + write `calls` edges (pass 2). Both sets may be pre-seeded (an
/// incremental reindex seeds them from the whole-corpus DB so calls into
/// unchanged files resolve). Records each file's `content_hash` in the ledger.
async fn extract_files(
    pool: &PgPool,
    corpus_id: Uuid,
    corpus_slug: &str,
    lang: &str,
    file_rows: &[FileRow],
    internal_fns: &mut HashSet<String>,
    internal_types: &mut HashSet<String>,
    generation: i64,
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
                generation,
                1.0,
                "ast",
            )
            .await?;
            sym_ids.insert(sym.qualified_name.clone(), id);
            idx_to_id.insert(i, id);
            stats.symbols += 1;
            if sym.node_type == "code:function" {
                internal_fns.insert(sym.qualified_name.clone());
            } else if matches!(
                sym.node_type,
                "code:struct" | "code:enum" | "code:impl" | "code:class" | "code:interface"
            ) {
                internal_types.insert(sym.qualified_name.clone());
            }

            // contains: parent (impl/mod) -> symbol, else file -> symbol.
            let src = match sym.parent.and_then(|p| idx_to_id.get(&p)) {
                Some(parent_id) => *parent_id,
                None => file_node_id,
            };
            if add_edge(pool, src, id, "contains", generation).await? {
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
                generation,
                1.0,
                "ast",
            )
            .await?;
            if add_edge(pool, file_node_id, imp_id, "imports", generation).await? {
                stats.imports += 1;
            }
        }

        // Persist this file's `pub use` re-exports so the corpus-wide facade map
        // (built below from the ledger) survives an incremental reindex that
        // didn't touch the owning lib.rs/mod.rs.
        record_file_reexports(pool, corpus_slug, &file_path, &parse).await?;

        pending.push(Pending {
            file_node_id,
            file_path,
            parse,
            sym_ids,
        });
    }

    // Corpus-wide `pub use` facade map + crate-root set for the facade redirect.
    // Loaded from the ledger, which pass 1 above just refreshed for every file in
    // this batch — so the map is whole-corpus and complete on BOTH a full reindex
    // and an incremental run (a changed file's facade call resolves even when the
    // owning lib.rs/mod.rs wasn't touched). Crate roots come from every known
    // internal fn (internal_fns is seeded whole-corpus on incremental too).
    let (mut reexports, mut glob_reexports) = load_reexports(pool, corpus_slug).await?;
    let crate_roots: HashSet<String> = internal_fns
        .iter()
        .filter_map(|f| f.split("::").next())
        .map(|s| s.to_string())
        .collect();
    // Recover re-export targets that `abs_reexport_target` mis-prefixed at parse
    // time. A cross-crate `pub use other_crate::path::item;` (or `::*;`) in module
    // `M` is stored as `M::other_crate::path::item` because the per-file parser
    // can't tell a workspace-crate head from a child submodule without the
    // whole-corpus crate set — which only exists HERE. Re-anchor any target that
    // carries a crate root in a non-head position back to the absolute crate path
    // (`ff_gateway::llm_routing::ff_core::model_id::normalize_model_id` ->
    // `ff_core::model_id::normalize_model_id`). No-op for correctly-stored rows: a
    // genuine cross-crate target already has its crate at position 0, and a pure
    // submodule re-export (`ff_db::leader_state`) carries no inner crate root.
    // Recall-neutral — same primitive lever #4 uses; a bad anchor just fails the
    // internal-fn check in `chase_reexport` and never fabricates an edge.
    for target in reexports.values_mut() {
        if let Some(anchored) = anchor_at_crate_root(target, &crate_roots) {
            *target = anchored;
        }
    }
    for targets in glob_reexports.values_mut() {
        for target in targets.iter_mut() {
            if let Some(anchored) = anchor_at_crate_root(target, &crate_roots) {
                *target = anchored;
            }
        }
    }
    // Every internal module path — a symbol's owning module plus all its
    // ancestors — for the external-head redirect (lever #4 denoise). It tells a
    // genuine child module of the caller (whose fabricated relative path is
    // CORRECT) apart from a bare 3rd-party crate / std primitive head the
    // resolver caller-prefixed onto the wrong module. Whole-corpus, same as the
    // other redirect maps (internal_fns/internal_types are seeded whole-corpus on
    // incremental too). For a symbol `a::b::c::leaf` this inserts `a`, `a::b`,
    // `a::b::c`.
    let mut internal_modules: HashSet<String> = HashSet::new();
    for qn in internal_fns.iter().chain(internal_types.iter()) {
        let Some((module, _leaf)) = qn.rsplit_once("::") else {
            continue;
        };
        let mut acc = String::new();
        for seg in module.split("::") {
            if !acc.is_empty() {
                acc.push_str("::");
            }
            acc.push_str(seg);
            internal_modules.insert(acc.clone());
        }
    }
    // Per-crate leaf index for the crate-root single-candidate redirect (lever
    // #7): `(crate, leaf)` -> every internal fn in that crate with that leaf, so
    // a missed sibling-module call `<crate>::<leaf>` can redirect when exactly one
    // candidate exists. Built whole-corpus (internal_fns is seeded whole-corpus on
    // incremental too), matching the other redirect maps.
    let mut crate_leaf_fns: HashMap<(String, String), Vec<String>> = HashMap::new();
    for f in internal_fns.iter() {
        let (Some(krate), Some(leaf)) = (f.split("::").next(), f.rsplit("::").next()) else {
            continue;
        };
        crate_leaf_fns
            .entry((krate.to_string(), leaf.to_string()))
            .or_default()
            .push(f.clone());
    }

    // Leaf index over internal TYPES for the Java nested-class constructor
    // redirect (J2): `<leaf>` -> every internal type whose last segment is that
    // leaf. A `new Inner()` call on a nested class is recorded as the caller-pkg
    // path `<caller_pkg>::Inner::Inner`, but the real class lives at
    // `<owner_pkg>::Outer::Inner` — the type segment lost its OUTER class. Suffix-
    // matching the leaf finds the real nested type. Whole-corpus (internal_types
    // is seeded whole-corpus on incremental too).
    let mut type_by_leaf: HashMap<String, Vec<String>> = HashMap::new();
    for t in internal_types.iter() {
        let Some(leaf) = t.rsplit("::").next() else {
            continue;
        };
        type_by_leaf
            .entry(leaf.to_string())
            .or_default()
            .push(t.clone());
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
            let mut resolved = resolve_call(&call.raw_path, &caller_qn, &p.parse);
            stats.calls_total += 1;
            // Provenance tier (roadmap #5): did the PRIMARY resolver already land
            // on a known internal fn? If so the edge is EXTRACTED (conf 1.0); if
            // only a heuristic redirect below lands it, it's INFERRED (0.6); if it
            // stays an extern, UNRESOLVED (0.3). Captured here, before any redirect.
            let extracted_internal = internal_fns.contains(&resolved);

            // Glob-import fallback: if the bare call didn't land on a known
            // internal fn, a `use <prefix>::*;` in scope may bring it in (the
            // `mod tests { use super::*; }` pattern). Redirect to the real symbol
            // instead of fabricating a code:extern in the caller's own module.
            if !internal_fns.contains(&resolved) {
                if let Some(g) =
                    resolve_glob_call(&call.raw_path, &caller_qn, &p.parse, internal_fns)
                {
                    resolved = g;
                }
            }

            // Wildcard-import fallback (Java/TS/Python): a bare call to a member
            // brought in by a `import static a.b.C.*;` / `import a.b.*;` /
            // `from m import *` resolved to `<caller_module>::<leaf>` (an extern
            // under the caller's path); redirect to the real `<base>::<leaf>` when
            // exactly one wildcard base names a known internal fn. The non-Rust
            // analogue of the glob-import fallback above.
            if !internal_fns.contains(&resolved) {
                if let Some(w) = resolve_wildcard_call(&call.raw_path, &p.parse, internal_fns) {
                    resolved = w;
                }
            }

            // Inherent-impl method redirect (Rust): an associated call
            // `Type::method()` resolves to the extern `module::Type::method`, but
            // the method is indexed flattened as `module::method` — redirect onto
            // the real method node instead of fabricating a per-type phantom.
            if p.parse.lang == Lang::Rust && !internal_fns.contains(&resolved) {
                if let Some(m) = resolve_impl_method_call(&resolved, internal_fns, internal_types) {
                    resolved = m;
                } else if let Some(m) = resolve_glob_impl_method_call(
                    &resolved,
                    &p.parse,
                    &caller_qn,
                    internal_fns,
                    internal_types,
                ) {
                    // lever #2b: `Type::method()` in a `use super::*` test module.
                    resolved = m;
                }
            }

            // `Self::method()` redirect (lever #5): an associated call on the
            // impl's own type lands on the fabricated extern
            // `<caller_module>::Self::method`; redirect onto the real flattened
            // method `<caller_module>::method`.
            if p.parse.lang == Lang::Rust && !internal_fns.contains(&resolved) {
                if let Some(m) = resolve_self_method_call(&resolved, internal_fns) {
                    resolved = m;
                }
            }

            // Facade redirect (levers #3/#4/#6): a call through a `pub use`
            // re-export path — named (`ff_db::run_postgres_migrations` for
            // `ff_db::migrations::run_postgres_migrations`) or glob (`ff_db::pg_get_current_leader`
            // re-exported by `pub use leader_state::*;`) — or one the resolver
            // caller-prefixed onto a crate-rooted path
            // (`ff_gateway::brain_api::ff_db::pg_get_brain_user`) lands on a
            // `code:extern`. Re-anchor at the crate root and chase the re-export
            // map to the real symbol. Redirect-only: fires only when the final
            // target is a known internal fn.
            if p.parse.lang == Lang::Rust && !internal_fns.contains(&resolved) {
                if let Some(t) = resolve_facade_call(
                    &resolved,
                    &reexports,
                    &glob_reexports,
                    &crate_roots,
                    internal_fns,
                    internal_types,
                ) {
                    resolved = t;
                }
            }

            // Crate-root single-candidate redirect (lever #7): a missed
            // sibling-module call `<crate>::<leaf>` (alias map missed the `use`)
            // where exactly one internal fn in the crate has that leaf. Runs last
            // — after facade, since a modeled re-export is the more specific match.
            if p.parse.lang == Lang::Rust && !internal_fns.contains(&resolved) {
                if let Some(t) = resolve_crate_root_call(&resolved, &crate_roots, &crate_leaf_fns) {
                    resolved = t;
                }
            }

            // External-crate / std-primitive head redirect (lever #4 denoise):
            // a bare 3rd-party crate or primitive in associated-call position
            // (`hex::decode`, `jsonwebtoken::decode`, `f64::deserialize`) that
            // isn't in the alias map / crate-name / hand-kept `looks_external`
            // denylist gets caller-prefixed by `resolve_call_inner` into
            // `<caller_module>::<head>::<leaf>` — an internally-rooted phantom
            // whose leaf collides with real fns and pollutes `ff cortex doctor`.
            // Collapse it back to the already-qualified `<head>::<leaf>` extern
            // (one shared node, not a per-caller-module phantom) when the head
            // isn't a real child module of the caller. Runs LAST — after
            // facade/crate-root, which try to find an INTERNAL target; this
            // handles the residual genuinely-external heads. Recall-neutral: it
            // fires only on a current extern and never rewrites an internally-
            // resolved edge (the leaf collision is removed, not re-pointed).
            if p.parse.lang == Lang::Rust && !internal_fns.contains(&resolved) {
                if let Some(t) =
                    resolve_external_head_call(&resolved, &caller_qn, &internal_modules)
                {
                    resolved = t;
                }
            }

            // Java constructor redirect: `new Foo()` lands on the extern
            // `<module>::Foo::Foo` when Foo has no explicit constructor symbol.
            // Point the instantiation edge at the class node instead. The target
            // is a TYPE (not a fn), so it's accepted by the callee lookup only via
            // this flag — internal_fns membership stays the fn-only test.
            let mut ctor_type_target = false;
            if p.parse.lang == Lang::Java && !internal_fns.contains(&resolved) {
                if let Some(t) = resolve_java_ctor_call(&resolved, internal_types, &type_by_leaf) {
                    resolved = t;
                    ctor_type_target = true;
                }
            }

            // Java nested-class method redirect (J2b): `Message.system(...)` on a
            // class nested in `AiClient` lands on the extern
            // `<caller_pkg>::Message::system`; the receiver type dropped its OUTER
            // class. Re-anchor onto the real `<owner_pkg>::AiClient::Message::system`
            // method node. Targets a fn (not a type), so no ctor_type_target.
            if p.parse.lang == Lang::Java && !internal_fns.contains(&resolved) {
                if let Some(t) =
                    resolve_java_method_call(&resolved, internal_fns, internal_types, &type_by_leaf)
                {
                    resolved = t;
                }
            }

            // Java enum-constant / nested-member method redirect (J2c):
            // `ErrorCode.INTERNAL_ERROR.status()` and an `@Nested` inner test
            // class calling an OUTER-class helper both record an extra qualifier
            // segment between the owning type and the method
            // (`<owner>::<Mid>::<leaf>`). Drop the middle segment to land on the
            // real method `<owner>::<leaf>`. Inverse of J2b (which adds the outer
            // class), so it runs after it.
            if p.parse.lang == Lang::Java && !internal_fns.contains(&resolved) {
                if let Some(t) =
                    resolve_java_member_method_call(&resolved, internal_fns, internal_types)
                {
                    resolved = t;
                }
            }

            // Find the callee node: internal real fn if known, else code:extern.
            // A Java constructor redirect resolves to an internal class/interface
            // node (a type, not a fn), so accept it via ctor_type_target.
            let callee_path = format!("code://{corpus_slug}/{resolved}");
            let callee_id = if internal_fns.contains(&resolved)
                || (ctor_type_target && internal_types.contains(&resolved))
            {
                // Internal fn (or a redirected constructor's class node): it has a
                // real code:* node somewhere.
                lookup_code_node(pool, &callee_path).await?
            } else {
                None
            };
            let (callee_id, confidence) = match callee_id {
                Some(id) => {
                    stats.calls_resolved += 1;
                    // EXTRACTED if the primary resolver named it; otherwise a
                    // redirect heuristic landed it → INFERRED.
                    if !extracted_internal {
                        stats.calls_inferred += 1;
                    }
                    (id, call_confidence(extracted_internal, true))
                }
                None => {
                    // External / unresolved: a code:extern placeholder on the same
                    // code:// path, so callers_of still traverses to it.
                    let confidence = call_confidence(extracted_internal, false);
                    let id = upsert_code_node(
                        pool,
                        &callee_path,
                        &resolved,
                        "code:extern",
                        corpus_slug,
                        None,
                        None,
                        generation,
                        confidence,
                        "ast",
                    )
                    .await?;
                    (id, confidence)
                }
            };
            add_call_edge(pool, caller_id, callee_id, confidence, generation).await?;
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
    /// Absolute path of the owning `content:file` node (None for extern/import
    /// placeholders that no file contains, or seed-resolution refs that aren't
    /// rendered). Resolved by [`resolve_ref_files`] at the public-query boundary.
    pub file: Option<String>,
    /// 1-based start line (None for pre-V124 nodes or non-spanning placeholders).
    pub start_line: Option<i32>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DbMigrationRef {
    pub title: String,
    pub edge_type: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DbField {
    pub column: String,
    pub corpus: String,
    pub table: String,
    pub descriptor: serde_json::Value,
    pub migrations: Vec<DbMigrationRef>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConfigSummary {
    pub key: String,
    pub corpus: String,
    pub node_type: String,
    pub readers: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConfigReader {
    pub qualified_name: String,
    pub path: String,
    pub confidence: f32,
    pub method: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConfigKeyDetail {
    pub key: String,
    pub corpus: String,
    pub node_type: String,
    pub readers: Vec<ConfigReader>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EventTopicSummary {
    pub subject: String,
    pub corpus: String,
    pub publishers: i64,
    pub subscribers: i64,
    pub one_sided: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EventEndpoint {
    pub qualified_name: String,
    pub path: String,
    pub confidence: f32,
    pub method: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EventTopicDetail {
    pub subject: String,
    pub corpus: String,
    pub publishers: Vec<EventEndpoint>,
    pub subscribers: Vec<EventEndpoint>,
    pub one_sided: bool,
}

/// Uniform "the symbol you asked about does not exist in this corpus" message,
/// shared by every symbol-keyed query (`callers`/`callees`/`impact`/`tests_for`).
/// These verbs resolve `sel` to a node first; an empty resolution means the
/// selector matched NOTHING (a typo or a stale index) — distinct from a symbol
/// that resolves fine but happens to have zero callers/callees/etc. The former
/// is an error (so the CLI exits non-zero and an agent can branch on it, like
/// `cortex show`/`outline` and `ff model where`); the latter is a legitimate
/// empty result (exit 0).
fn no_symbol_error(sel: &str, corpus_slug: &str) -> anyhow::Error {
    anyhow::anyhow!("no symbol matching '{sel}' in corpus '{corpus_slug}'")
}

/// Resolve a database column extracted by the `db_schema` Cortex dimension.
pub async fn field(pool: &PgPool, corpus_slug: Option<&str>, column: &str) -> Result<Vec<DbField>> {
    let rows = sqlx::query(
        r#"SELECT c.id, c.title, c.project, t.title AS table_title,
                  COALESCE(e.evidence, '{}'::jsonb) AS descriptor
             FROM brain_vault_nodes c
             JOIN brain_vault_edges e ON e.dst_id = c.id AND e.edge_type = 'has_column'
             JOIN brain_vault_nodes t ON t.id = e.src_id AND t.node_type = 'db:table'
            WHERE c.node_type = 'db:column'
              AND c.title = $1
              AND ($2::text IS NULL OR c.project = $2)
              AND COALESCE(c.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = c.project), 0)
              )
              AND COALESCE(e.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = c.project), 0)
              )
            ORDER BY c.project, c.title"#,
    )
    .bind(column)
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: Uuid = row.get("id");
        let corpus: String = row.get("project");
        let migrations = sqlx::query(
            r#"SELECT DISTINCT m.title, e.edge_type
                 FROM brain_vault_edges e
                 JOIN brain_vault_nodes m ON m.id = e.src_id
                WHERE e.dst_id = $1
                  AND m.node_type = 'db:migration'
                  AND e.edge_type IN ('creates', 'alters', 'drops')
                  AND COALESCE(e.generation, 0) IN (
                      0,
                      COALESCE((SELECT current_generation
                                  FROM cortex_generations
                                 WHERE project = m.project), 0)
                  )
                ORDER BY m.title, e.edge_type"#,
        )
        .bind(id)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| DbMigrationRef {
            title: r.get("title"),
            edge_type: r.get("edge_type"),
        })
        .collect();

        out.push(DbField {
            column: row.get("title"),
            corpus,
            table: row.get("table_title"),
            descriptor: row.get("descriptor"),
            migrations,
        });
    }
    Ok(out)
}

/// List configuration keys extracted from env/secret/feature-flag access sites.
pub async fn config(pool: &PgPool, corpus_slug: Option<&str>) -> Result<Vec<ConfigSummary>> {
    let rows = sqlx::query(
        r#"SELECT c.title AS key,
                  c.project AS corpus,
                  c.node_type,
                  COUNT(DISTINCT r.src_id) AS readers
             FROM brain_vault_nodes c
             LEFT JOIN brain_vault_edges r
                    ON r.dst_id = c.id
                   AND r.edge_type = 'reads_config'
                   AND COALESCE(r.generation, 0) IN (
                       0,
                       COALESCE((SELECT current_generation
                                   FROM cortex_generations
                                  WHERE project = c.project), 0)
                   )
            WHERE c.node_type LIKE 'config:%'
              AND ($1::text IS NULL OR c.project = $1)
              AND COALESCE(c.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = c.project), 0)
              )
            GROUP BY c.title, c.project, c.node_type
            ORDER BY c.project, c.node_type, c.title"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| ConfigSummary {
            key: row.get("key"),
            corpus: row.get("corpus"),
            node_type: row.get("node_type"),
            readers: row.get("readers"),
        })
        .collect())
}

/// Show which functions read one extracted config key.
pub async fn config_key(
    pool: &PgPool,
    corpus_slug: Option<&str>,
    key: &str,
) -> Result<Vec<ConfigKeyDetail>> {
    let config_rows = sqlx::query(
        r#"SELECT c.id, c.title AS key, c.project AS corpus, c.node_type
             FROM brain_vault_nodes c
            WHERE c.node_type LIKE 'config:%'
              AND c.title = $1
              AND ($2::text IS NULL OR c.project = $2)
              AND COALESCE(c.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = c.project), 0)
              )
            ORDER BY c.project, c.node_type, c.title"#,
    )
    .bind(key)
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(config_rows.len());
    for row in config_rows {
        let id: Uuid = row.get("id");
        let corpus: String = row.get("corpus");
        let readers = sqlx::query(
            r#"SELECT f.title AS qualified_name,
                      f.path,
                      r.confidence,
                      r.method
                 FROM brain_vault_edges r
                 JOIN brain_vault_nodes f ON f.id = r.src_id
                WHERE r.dst_id = $1
                  AND r.edge_type = 'reads_config'
                  AND f.node_type = 'code:function'
                  AND COALESCE(r.generation, 0) IN (
                      0,
                      COALESCE((SELECT current_generation
                                  FROM cortex_generations
                                 WHERE project = $2), 0)
                  )
                  AND f.valid_until IS NULL
                ORDER BY f.title"#,
        )
        .bind(id)
        .bind(&corpus)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| ConfigReader {
            qualified_name: r.get("qualified_name"),
            path: r.get("path"),
            confidence: r.get("confidence"),
            method: r.get("method"),
        })
        .collect();

        out.push(ConfigKeyDetail {
            key: row.get("key"),
            corpus,
            node_type: row.get("node_type"),
            readers,
        });
    }
    Ok(out)
}

/// List event topics extracted from NATS / message-bus publish-subscribe sites.
pub async fn topics(pool: &PgPool, corpus_slug: Option<&str>) -> Result<Vec<EventTopicSummary>> {
    let rows = sqlx::query(
        r#"SELECT t.title AS subject,
                  t.project AS corpus,
                  COUNT(DISTINCT p.src_id) FILTER (WHERE p.edge_type = 'publishes') AS publishers,
                  COUNT(DISTINCT s.src_id) FILTER (WHERE s.edge_type = 'subscribes') AS subscribers
             FROM brain_vault_nodes t
             LEFT JOIN brain_vault_edges p
                    ON p.dst_id = t.id
                   AND p.edge_type = 'publishes'
                   AND COALESCE(p.generation, 0) IN (
                       0,
                       COALESCE((SELECT current_generation
                                   FROM cortex_generations
                                  WHERE project = t.project), 0)
                   )
             LEFT JOIN brain_vault_edges s
                    ON s.dst_id = t.id
                   AND s.edge_type = 'subscribes'
                   AND COALESCE(s.generation, 0) IN (
                       0,
                       COALESCE((SELECT current_generation
                                   FROM cortex_generations
                                  WHERE project = t.project), 0)
                   )
            WHERE t.node_type = 'event:topic'
              AND ($1::text IS NULL OR t.project = $1)
              AND COALESCE(t.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = t.project), 0)
              )
            GROUP BY t.title, t.project
            ORDER BY t.project, t.title"#,
    )
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let publishers: i64 = r.get("publishers");
            let subscribers: i64 = r.get("subscribers");
            EventTopicSummary {
                subject: r.get("subject"),
                corpus: r.get("corpus"),
                publishers,
                subscribers,
                one_sided: (publishers == 0) != (subscribers == 0),
            }
        })
        .collect())
}

/// Inspect publishers/subscribers for one extracted event subject.
pub async fn topic(
    pool: &PgPool,
    corpus_slug: Option<&str>,
    subject: &str,
) -> Result<Vec<EventTopicDetail>> {
    let topics = sqlx::query(
        r#"SELECT id, title, project
             FROM brain_vault_nodes
            WHERE node_type = 'event:topic'
              AND title = $1
              AND ($2::text IS NULL OR project = $2)
              AND COALESCE(generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = brain_vault_nodes.project), 0)
              )
            ORDER BY project, title"#,
    )
    .bind(subject)
    .bind(corpus_slug)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(topics.len());
    for topic_row in topics {
        let id: Uuid = topic_row.get("id");
        let corpus: String = topic_row.get("project");
        let publishers = event_endpoints(pool, id, "publishes").await?;
        let subscribers = event_endpoints(pool, id, "subscribes").await?;
        out.push(EventTopicDetail {
            subject: topic_row.get("title"),
            corpus,
            one_sided: publishers.is_empty() != subscribers.is_empty(),
            publishers,
            subscribers,
        });
    }
    Ok(out)
}

async fn event_endpoints(
    pool: &PgPool,
    topic_id: Uuid,
    edge_type: &str,
) -> Result<Vec<EventEndpoint>> {
    let rows = sqlx::query(
        r#"SELECT DISTINCT n.title, n.path, e.confidence, e.method
             FROM brain_vault_edges e
             JOIN brain_vault_nodes n ON n.id = e.src_id
            WHERE e.dst_id = $1
              AND e.edge_type = $2
              AND n.node_type = 'code:function'
              AND COALESCE(e.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = n.project), 0)
              )
            ORDER BY n.title"#,
    )
    .bind(topic_id)
    .bind(edge_type)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| EventEndpoint {
            qualified_name: r.get("title"),
            path: r.get("path"),
            confidence: r.get("confidence"),
            method: r.get("method"),
        })
        .collect())
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
              AND COALESCE(generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = brain_vault_nodes.project), 0)
              )
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
            file: None,
            start_line: None,
        })
        .collect())
}

/// Fill each ref's owning `content:file` path (the same `contains`-walk as
/// [`resolve_hit_files`], for the relationship verbs). `start_line` is already
/// populated from the node row; this adds `file` so `callers`/`callees`/`impact`
/// output is directly actionable (`file:line`) without a second `find`/`show`.
async fn resolve_ref_files(pool: &PgPool, refs: &mut [SymbolRef]) -> Result<()> {
    let ids: Vec<Uuid> = refs.iter().map(|r| r.id).collect();
    let by_leaf = owning_files(pool, &ids).await?;
    for r in refs.iter_mut() {
        r.file = by_leaf.get(&r.id).cloned();
    }
    Ok(())
}

/// Direct callers of a set of symbol node ids: nodes with a `calls` edge whose
/// dst is one of the ids. Querying by id (not by name selector) is exact — no
/// bare-leaf ambiguity — which matters for review, where the ids come straight
/// from a file's `contains` subtree.
async fn callers_of_ids(
    pool: &PgPool,
    ids: &[Uuid],
    min_confidence: f32,
) -> Result<Vec<SymbolRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        r#"SELECT DISTINCT n.id, n.title, n.node_type, n.start_line
             FROM brain_vault_edges e
             JOIN brain_vault_nodes n ON n.id = e.src_id
            WHERE e.edge_type = 'calls' AND e.dst_id = ANY($1)
              AND e.confidence >= $2
              AND COALESCE(e.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = n.project), 0)
              )
            ORDER BY n.title"#,
    )
    .bind(ids)
    .bind(min_confidence)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SymbolRef {
            id: r.get("id"),
            qualified_name: r.get("title"),
            node_type: r.get("node_type"),
            file: None,
            start_line: r.get("start_line"),
        })
        .collect())
}

/// Callers of a symbol: nodes with a `calls` edge whose dst is the symbol.
///
/// `min_confidence` filters the traversed `calls` edges by their resolution-
/// confidence tier (roadmap #5): `0.0` keeps every edge, `1.0` keeps only
/// EXTRACTED edges (primary resolver named a real internal symbol), `0.6` adds
/// INFERRED (heuristic redirect). A high-precision consumer passes `1.0` to drop
/// the ~40% of edges that come from heuristic redirects.
pub async fn callers(
    pool: &PgPool,
    corpus_slug: &str,
    sel: &str,
    min_confidence: f32,
) -> Result<Vec<SymbolRef>> {
    let targets = resolve_symbol(pool, corpus_slug, sel).await?;
    if targets.is_empty() {
        return Err(no_symbol_error(sel, corpus_slug));
    }
    let ids: Vec<Uuid> = targets.iter().map(|t| t.id).collect();
    let mut out = callers_of_ids(pool, &ids, min_confidence).await?;
    resolve_ref_files(pool, &mut out).await?;
    Ok(out)
}

/// Callees of a symbol: nodes a `calls` edge points to from the symbol.
/// `min_confidence` filters edges by tier — see [`callers`].
pub async fn callees(
    pool: &PgPool,
    corpus_slug: &str,
    sel: &str,
    min_confidence: f32,
) -> Result<Vec<SymbolRef>> {
    let srcs = resolve_symbol(pool, corpus_slug, sel).await?;
    if srcs.is_empty() {
        return Err(no_symbol_error(sel, corpus_slug));
    }
    let ids: Vec<Uuid> = srcs.iter().map(|t| t.id).collect();
    let rows = sqlx::query(
        r#"SELECT DISTINCT n.id, n.title, n.node_type, n.start_line
             FROM brain_vault_edges e
             JOIN brain_vault_nodes n ON n.id = e.dst_id
            WHERE e.edge_type = 'calls' AND e.src_id = ANY($1)
              AND e.confidence >= $2
              AND COALESCE(e.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = n.project), 0)
              )
            ORDER BY n.title"#,
    )
    .bind(&ids)
    .bind(min_confidence)
    .fetch_all(pool)
    .await?;
    let mut out: Vec<SymbolRef> = rows
        .into_iter()
        .map(|r| SymbolRef {
            id: r.get("id"),
            qualified_name: r.get("title"),
            node_type: r.get("node_type"),
            file: None,
            start_line: r.get("start_line"),
        })
        .collect();
    resolve_ref_files(pool, &mut out).await?;
    Ok(out)
}

/// Edge endpoints for a cross-corpus call query. For CALLERS we want rows where
/// the definition is the `dst` (it is called) and the matched node is the `src`
/// (the caller); for CALLEES it's the reverse. Pure — pinned by a unit test.
fn cross_corpus_edge_cols(want_callers: bool) -> (&'static str, &'static str) {
    if want_callers {
        ("e.src_id", "e.dst_id") // (node-to-return, def-filter)
    } else {
        ("e.dst_id", "e.src_id")
    }
}

/// Cross-repo `callers`/`callees`: resolve a bare symbol NAME to its definitions
/// in EVERY indexed corpus, then return its callers (`want_callers=true`) or
/// callees (false) across all corpora — each tagged with the OWNING corpus.
/// Answers "who calls `foo` anywhere in the fleet's codebases?".
///
/// Caveat: `calls` edges are intra-corpus (tree-sitter resolves within a repo),
/// so this UNIONS the per-corpus callers of every same-named definition — it
/// does NOT resolve a call in repo A to a definition that lives only in repo B.
async fn cross_corpus_call_refs(
    pool: &PgPool,
    sel: &str,
    min_confidence: f32,
    want_callers: bool,
) -> Result<Vec<(String, SymbolRef)>> {
    // 1. Definitions named `sel` in any corpus (generation-live). Titles are
    //    QUALIFIED, so match the bare name OR a `…::sel` suffix (mirrors
    //    resolve_symbol's bare-name handling).
    let def_ids: Vec<Uuid> = sqlx::query_scalar(
        r#"SELECT id FROM brain_vault_nodes
            WHERE node_type LIKE 'code:%'
              AND (title = $1 OR title LIKE $2)
              AND COALESCE(generation, 0) IN (
                  0, COALESCE((SELECT current_generation FROM cortex_generations
                                WHERE project = brain_vault_nodes.project), 0))"#,
    )
    .bind(sel)
    .bind(format!("%::{sel}"))
    .fetch_all(pool)
    .await?;
    if def_ids.is_empty() {
        return Ok(Vec::new());
    }
    // 2. Callers/callees, tagged with the matched node's project.
    let (node_col, def_col) = cross_corpus_edge_cols(want_callers);
    let sql = format!(
        r#"SELECT DISTINCT n.id, n.project, n.title, n.node_type, n.start_line
             FROM brain_vault_edges e
             JOIN brain_vault_nodes n ON n.id = {node_col}
            WHERE e.edge_type = 'calls' AND {def_col} = ANY($1)
              AND e.confidence >= $2
              AND COALESCE(e.generation, 0) IN (
                  0, COALESCE((SELECT current_generation FROM cortex_generations
                                WHERE project = n.project), 0))
            ORDER BY n.project, n.title"#
    );
    let rows = sqlx::query(&sql)
        .bind(&def_ids)
        .bind(min_confidence)
        .fetch_all(pool)
        .await?;
    let mut projects = Vec::with_capacity(rows.len());
    let mut refs = Vec::with_capacity(rows.len());
    for r in rows {
        projects.push(r.get::<String, _>("project"));
        refs.push(SymbolRef {
            id: r.get("id"),
            qualified_name: r.get("title"),
            node_type: r.get("node_type"),
            file: None,
            start_line: r.get("start_line"),
        });
    }
    resolve_ref_files(pool, &mut refs).await?;
    Ok(projects.into_iter().zip(refs).collect())
}

/// Cross-repo callers of `sel` (every corpus), each tagged with its corpus.
pub async fn callers_all_corpora(
    pool: &PgPool,
    sel: &str,
    min_confidence: f32,
) -> Result<Vec<(String, SymbolRef)>> {
    cross_corpus_call_refs(pool, sel, min_confidence, true).await
}

/// Cross-repo callees of `sel` (every corpus), each tagged with its corpus.
pub async fn callees_all_corpora(
    pool: &PgPool,
    sel: &str,
    min_confidence: f32,
) -> Result<Vec<(String, SymbolRef)>> {
    cross_corpus_call_refs(pool, sel, min_confidence, false).await
}

/// Transitive caller closure up to `max_depth` (impact / blast radius).
pub async fn impact(
    pool: &PgPool,
    corpus_slug: &str,
    sel: &str,
    max_depth: usize,
    min_confidence: f32,
) -> Result<Vec<SymbolRef>> {
    let seed = resolve_symbol(pool, corpus_slug, sel).await?;
    if seed.is_empty() {
        return Err(no_symbol_error(sel, corpus_slug));
    }
    let seed_ids: Vec<Uuid> = seed.iter().map(|s| s.id).collect();
    let mut out = impact_of_ids(pool, &seed_ids, max_depth, min_confidence).await?;
    resolve_ref_files(pool, &mut out).await?;
    Ok(out)
}

/// Transitive caller closure of a set of seed node ids (the seeds themselves are
/// excluded from the result). Shared by [`impact`] and the review pass.
/// `min_confidence` filters traversed `calls` edges by tier — see [`callers`].
async fn impact_of_ids(
    pool: &PgPool,
    seed_ids: &[Uuid],
    max_depth: usize,
    min_confidence: f32,
) -> Result<Vec<SymbolRef>> {
    let mut frontier: Vec<Uuid> = seed_ids.to_vec();
    let mut seen: HashSet<Uuid> = frontier.iter().copied().collect();
    let mut out: Vec<SymbolRef> = Vec::new();

    for _ in 0..max_depth {
        if frontier.is_empty() {
            break;
        }
        let rows = sqlx::query(
            r#"SELECT DISTINCT n.id, n.title, n.node_type, n.start_line
                 FROM brain_vault_edges e
                 JOIN brain_vault_nodes n ON n.id = e.src_id
                WHERE e.edge_type = 'calls' AND e.dst_id = ANY($1)
                  AND e.confidence >= $2"#,
        )
        .bind(&frontier)
        .bind(min_confidence)
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
                    file: None,
                    start_line: r.get("start_line"),
                });
                next.push(id);
            }
        }
        frontier = next;
    }
    out.sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name));
    Ok(out)
}

// ─── Call-path search (ff cortex path FROM TO) ───────────────────────────────
//
// `callers`/`callees` answer one hop; `impact` answers "everything that reaches
// X". Neither answers "HOW does A reach B" — the call chain connecting two
// symbols. That's a shortest path over the directed `calls` subgraph (forward
// edges src→dst). A breadth-first search from FROM yields the *fewest-hop* chain,
// which is the most useful single answer ("the shortest way control flows from A
// to B"). Read-only, additive — same `min_confidence` tier filter as the other
// traversal verbs.

/// Reconstruct the ordered id chain FROM → … → TO by walking `parent` pointers
/// back from `target` until a source (a node with no parent entry). Pure so the
/// path-stitching logic is unit-testable without a database. Returns the chain in
/// forward order (source first, `target` last).
fn reconstruct_path(parent: &HashMap<Uuid, Uuid>, target: Uuid) -> Vec<Uuid> {
    let mut chain = vec![target];
    let mut cur = target;
    // `parent` is built by BFS, which never revisits a node, so the walk is
    // acyclic and bounded by the number of discovered nodes.
    while let Some(&p) = parent.get(&cur) {
        chain.push(p);
        cur = p;
    }
    chain.reverse();
    chain
}

/// Fetch `SymbolRef`s for a list of node ids, preserving the input order (so a
/// reconstructed path renders FROM → … → TO). Ids with no surviving node are
/// dropped.
async fn fetch_refs_ordered(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<SymbolRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        r#"SELECT id, title, node_type, start_line
             FROM brain_vault_nodes WHERE id = ANY($1)"#,
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    let by_id: HashMap<Uuid, SymbolRef> = rows
        .into_iter()
        .map(|r| {
            let id: Uuid = r.get("id");
            (
                id,
                SymbolRef {
                    id,
                    qualified_name: r.get("title"),
                    node_type: r.get("node_type"),
                    file: None,
                    start_line: r.get("start_line"),
                },
            )
        })
        .collect();
    Ok(ids.iter().filter_map(|id| by_id.get(id).cloned()).collect())
}

/// Shortest call path from any symbol matching `from_sel` to any symbol matching
/// `to_sel`, over the directed `calls` subgraph (forward, src→dst). Returns the
/// ordered chain FROM → … → TO (each hop a real `calls` edge), or `Ok(None)` when
/// no path exists within `max_depth` hops. Errors only if `from_sel`/`to_sel`
/// resolve to nothing (typo / stale index) — distinct from "they exist but don't
/// connect", which is the legitimate `None`.
///
/// `min_confidence` filters traversed edges by resolution tier — see [`callers`].
pub async fn call_path(
    pool: &PgPool,
    corpus_slug: &str,
    from_sel: &str,
    to_sel: &str,
    max_depth: usize,
    min_confidence: f32,
) -> Result<Option<Vec<SymbolRef>>> {
    let from = resolve_symbol(pool, corpus_slug, from_sel).await?;
    if from.is_empty() {
        return Err(no_symbol_error(from_sel, corpus_slug));
    }
    let to = resolve_symbol(pool, corpus_slug, to_sel).await?;
    if to.is_empty() {
        return Err(no_symbol_error(to_sel, corpus_slug));
    }
    let from_ids: Vec<Uuid> = from.iter().map(|s| s.id).collect();
    let to_set: HashSet<Uuid> = to.iter().map(|s| s.id).collect();

    // FROM and TO overlap (e.g. a bare leaf matched the same node): the chain is
    // that single node — a zero-hop path.
    if let Some(hit) = from_ids.iter().find(|id| to_set.contains(id)) {
        let mut refs = fetch_refs_ordered(pool, &[*hit]).await?;
        resolve_ref_files(pool, &mut refs).await?;
        return Ok(Some(refs));
    }

    // BFS forward from every FROM node simultaneously, recording the first parent
    // that discovers each node (first discovery = shortest, by BFS layering).
    let mut parent: HashMap<Uuid, Uuid> = HashMap::new();
    let mut seen: HashSet<Uuid> = from_ids.iter().copied().collect();
    let mut frontier: Vec<Uuid> = from_ids.clone();

    for _ in 0..max_depth {
        if frontier.is_empty() {
            break;
        }
        let rows = sqlx::query(
            r#"SELECT e.src_id, e.dst_id
                 FROM brain_vault_edges e
                WHERE e.edge_type = 'calls' AND e.src_id = ANY($1)
                  AND e.confidence >= $2"#,
        )
        .bind(&frontier)
        .bind(min_confidence)
        .fetch_all(pool)
        .await?;
        let mut next: Vec<Uuid> = Vec::new();
        for r in rows {
            let src: Uuid = r.get("src_id");
            let dst: Uuid = r.get("dst_id");
            if seen.insert(dst) {
                parent.insert(dst, src);
                if to_set.contains(&dst) {
                    let chain = reconstruct_path(&parent, dst);
                    let mut refs = fetch_refs_ordered(pool, &chain).await?;
                    resolve_ref_files(pool, &mut refs).await?;
                    return Ok(Some(refs));
                }
                next.push(dst);
            }
        }
        frontier = next;
    }
    Ok(None)
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
    /// 1-based start line of the test (None for pre-V124 nodes).
    pub start_line: Option<i32>,
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
///
/// `min_confidence` filters the traversed `calls` edges by their resolution-
/// confidence tier (roadmap #5; see [`callers`]): `0.0` keeps every edge (full
/// recall — the default), `1.0` keeps only EXTRACTED edges so the result is the
/// stricter "tests that *provably* reach this symbol via a directly-resolved call
/// chain", dropping coverage that only exists through a heuristic redirect.
pub async fn tests_for(
    pool: &PgPool,
    corpus_slug: &str,
    sel: &str,
    max_depth: usize,
    min_confidence: f32,
) -> Result<Vec<TestHit>> {
    let seed = resolve_symbol(pool, corpus_slug, sel).await?;
    if seed.is_empty() {
        return Err(no_symbol_error(sel, corpus_slug));
    }
    let max_depth = max_depth.clamp(1, 20);
    let seed_ids: Vec<Uuid> = seed.iter().map(|s| s.id).collect();

    // Reverse BFS over `calls` edges, recording the depth at which each caller is
    // first reached (its shortest call distance from the seed).
    let mut frontier: Vec<Uuid> = seed_ids.clone();
    let mut seen: HashSet<Uuid> = frontier.iter().copied().collect();
    let mut callers: Vec<(Uuid, String, Option<i32>, usize)> = Vec::new();
    for depth in 1..=max_depth {
        if frontier.is_empty() {
            break;
        }
        let rows = sqlx::query(
            r#"SELECT DISTINCT n.id, n.title, n.start_line
                 FROM brain_vault_edges e
                 JOIN brain_vault_nodes n ON n.id = e.src_id
                WHERE e.edge_type = 'calls' AND e.dst_id = ANY($1)
                  AND e.confidence >= $2
                  AND COALESCE(e.generation, 0) IN (
                      0,
                      COALESCE((SELECT current_generation
                                  FROM cortex_generations
                                 WHERE project = n.project), 0)
                  )
                  AND n.node_type = 'code:function'"#,
        )
        .bind(&frontier)
        .bind(min_confidence)
        .fetch_all(pool)
        .await?;
        let mut next: Vec<Uuid> = Vec::new();
        for r in rows {
            let id: Uuid = r.get("id");
            if seen.insert(id) {
                callers.push((id, r.get("title"), r.get("start_line"), depth));
                next.push(id);
            }
        }
        frontier = next;
    }

    // Resolve owning files once, then keep only the test callers.
    let ids: Vec<Uuid> = callers.iter().map(|(id, _, _, _)| *id).collect();
    let files = owning_files(pool, &ids).await?;
    let mut out: Vec<TestHit> = callers
        .into_iter()
        .filter_map(|(id, qn, start_line, depth)| {
            let file = files.get(&id).cloned();
            if is_test_symbol(&qn, file.as_deref()) {
                Some(TestHit {
                    id,
                    qualified_name: qn,
                    file,
                    start_line,
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
              AND COALESCE(n.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = n.project), 0)
              )
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

/// Cross-corpus variant of [`find_symbols`]: search EVERY indexed corpus at once
/// and tag each hit with its project slug. The monorepo / multi-repo navigation
/// entrypoint — "where does `foo` live across all my repos?". Same liveness +
/// generation rules (per-project `current_generation`, so a corpus with stranded
/// generations is handled identically) and the same fan-in ranking, then grouped
/// by project for a stable read. Returns `(project_slug, hit)` pairs.
pub async fn find_symbols_all_corpora(
    pool: &PgPool,
    query: &str,
    limit: i64,
    kind: Option<&str>,
) -> Result<Vec<(String, SymbolHit)>> {
    let pattern = format!("%{}%", escape_like(query));
    let limit = limit.clamp(1, 500);
    let kind_types = resolve_kind_filter(kind)?;
    let rows = sqlx::query(
        r#"SELECT n.id, n.project, n.title, n.node_type, n.start_line,
                  (SELECT count(*) FROM brain_vault_edges e
                    WHERE e.edge_type = 'calls' AND e.dst_id = n.id) AS fan_in
             FROM brain_vault_nodes n
            WHERE n.node_type LIKE 'code:%'
              AND n.title ILIKE $1 ESCAPE '\'
              AND ($3::text[] IS NULL OR n.node_type = ANY($3))
              AND COALESCE(n.generation, 0) IN (
                  0,
                  COALESCE((SELECT current_generation
                              FROM cortex_generations
                             WHERE project = n.project), 0)
              )
            ORDER BY fan_in DESC, n.project, n.title COLLATE "C"
            LIMIT $2"#,
    )
    .bind(&pattern)
    .bind(limit)
    .bind(kind_types.as_deref())
    .fetch_all(pool)
    .await?;

    let projects: Vec<String> = rows.iter().map(|r| r.get::<String, _>("project")).collect();
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
    Ok(projects.into_iter().zip(hits).collect())
}

/// Static code-health signals derived purely from the Cortex graph (no LLM).
/// Returned by [`cortex_alerts`].
#[derive(Debug, Clone)]
pub struct CortexAlerts {
    /// Refactor-risk hotspots: most-depended-on functions `(qualified_name,
    /// fan_in)`, fan-in desc. Changing these ripples widely.
    pub god_symbols: Vec<(String, i64)>,
    /// Mutually-importing pairs `(a, b)` — the worst import cycles. Empty = none.
    pub circular_imports: Vec<(String, String)>,
    /// Total count of functions with NO in-graph call edges (no callers and no
    /// callees), plus a capped sample. LOW-CONFIDENCE: a heuristic call graph
    /// can't see macro/attribute/framework invocation (serde `default=`, derive,
    /// route handlers), so many of these are actually used — review, don't delete.
    pub isolated_total: i64,
    pub isolated_sample: Vec<String>,
}

/// Compute [`CortexAlerts`] for a corpus from the live graph. `limit` caps each
/// list. Only the current generation's live (`valid_until IS NULL`) code nodes.
pub async fn cortex_alerts(pool: &PgPool, corpus_slug: &str, limit: i64) -> Result<CortexAlerts> {
    let limit = limit.clamp(1, 200);

    // Generation/liveness filter shared by all three queries.
    let gen_filter = "COALESCE(n.generation, 0) IN (0, COALESCE((SELECT current_generation \
               FROM cortex_generations WHERE project = n.project), 0))";

    let god_rows = sqlx::query(&format!(
        "SELECT n.title,
                (SELECT count(*) FROM brain_vault_edges e
                  WHERE e.edge_type = 'calls' AND e.dst_id = n.id) AS fan_in
           FROM brain_vault_nodes n
          WHERE n.project = $1 AND n.node_type = 'code:function'
            AND n.valid_until IS NULL AND {gen_filter}
          ORDER BY fan_in DESC, n.title COLLATE \"C\"
          LIMIT $2"
    ))
    .bind(corpus_slug)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let god_symbols = god_rows
        .into_iter()
        .map(|r| (r.get::<String, _>("title"), r.get::<i64, _>("fan_in")))
        .filter(|(_, f)| *f > 0)
        .collect();

    // Mutual imports: src<dst dedupes the symmetric pair.
    let cyc_rows = sqlx::query(
        r#"SELECT s.title AS a, d.title AS b
             FROM brain_vault_edges ea
             JOIN brain_vault_edges eb ON ea.src_id = eb.dst_id AND ea.dst_id = eb.src_id
             JOIN brain_vault_nodes s ON s.id = ea.src_id
             JOIN brain_vault_nodes d ON d.id = ea.dst_id
            WHERE ea.edge_type = 'imports' AND eb.edge_type = 'imports'
              AND ea.src_id < ea.dst_id
              AND s.project = $1 AND d.project = $1
            ORDER BY s.title COLLATE "C"
            LIMIT $2"#,
    )
    .bind(corpus_slug)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let circular_imports = cyc_rows
        .into_iter()
        .map(|r| (r.get::<String, _>("a"), r.get::<String, _>("b")))
        .collect();

    let iso_filter = format!(
        "n.project = $1 AND n.node_type = 'code:function' AND n.valid_until IS NULL AND {gen_filter} \
         AND n.title NOT ILIKE '%test%' AND n.title NOT ILIKE '%::main' \
         AND NOT EXISTS (SELECT 1 FROM brain_vault_edges e WHERE e.edge_type='calls' AND e.dst_id=n.id) \
         AND NOT EXISTS (SELECT 1 FROM brain_vault_edges e WHERE e.edge_type='calls' AND e.src_id=n.id)"
    );
    let isolated_total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM brain_vault_nodes n WHERE {iso_filter}"
    ))
    .bind(corpus_slug)
    .fetch_one(pool)
    .await?;
    let isolated_sample: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT n.title FROM brain_vault_nodes n WHERE {iso_filter} ORDER BY n.title COLLATE \"C\" LIMIT $2"
    ))
    .bind(corpus_slug)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(CortexAlerts {
        god_symbols,
        circular_imports,
        isolated_total,
        isolated_sample,
    })
}

/// A small graph slice for visualization export ([`export_neighborhood`]).
#[derive(Debug, Clone, Default)]
pub struct GraphExport {
    /// `(id, label, kind)` — id is the node UUID (stable), label the qualified
    /// name, kind the `code:*` type without the prefix.
    pub nodes: Vec<(String, String, String)>,
    /// `(source_id, target_id, edge_type)` directed edges among the nodes.
    pub edges: Vec<(String, String, String)>,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Serialize a [`GraphExport`] to GraphML (the interchange format yEd / Gephi /
/// Cytoscape read). Pure; unit-tested.
pub fn graph_to_graphml(g: &GraphExport) -> String {
    let mut o = String::with_capacity(256 + g.nodes.len() * 96 + g.edges.len() * 80);
    o.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    o.push_str("<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\">\n");
    o.push_str("  <key id=\"label\" for=\"node\" attr.name=\"label\" attr.type=\"string\"/>\n");
    o.push_str("  <key id=\"kind\" for=\"node\" attr.name=\"kind\" attr.type=\"string\"/>\n");
    o.push_str("  <key id=\"etype\" for=\"edge\" attr.name=\"type\" attr.type=\"string\"/>\n");
    o.push_str("  <graph edgedefault=\"directed\">\n");
    for (id, label, kind) in &g.nodes {
        o.push_str(&format!(
            "    <node id=\"{}\"><data key=\"label\">{}</data><data key=\"kind\">{}</data></node>\n",
            xml_escape(id),
            xml_escape(label),
            xml_escape(kind)
        ));
    }
    for (src, dst, etype) in &g.edges {
        o.push_str(&format!(
            "    <edge source=\"{}\" target=\"{}\"><data key=\"etype\">{}</data></edge>\n",
            xml_escape(src),
            xml_escape(dst),
            xml_escape(etype)
        ));
    }
    o.push_str("  </graph>\n</graphml>\n");
    o
}

/// Serialize a [`GraphExport`] to Graphviz DOT (render with `dot -Tsvg`). Pure;
/// unit-tested. Labels (not ids) are shown; ids key the edges.
pub fn graph_to_dot(g: &GraphExport) -> String {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let mut o = String::from("digraph cortex {\n  rankdir=LR;\n  node [shape=box, fontsize=10];\n");
    for (id, label, _kind) in &g.nodes {
        o.push_str(&format!("  \"{}\" [label=\"{}\"];\n", esc(id), esc(label)));
    }
    for (src, dst, _etype) in &g.edges {
        o.push_str(&format!("  \"{}\" -> \"{}\";\n", esc(src), esc(dst)));
    }
    o.push_str("}\n");
    o
}

/// Export the call-graph neighborhood of a symbol (callers + callees within
/// `depth` hops, capped at `max_nodes` nearest by hop distance) as a
/// [`GraphExport`] for visualization. Resolves `symbol` like the other
/// `find_symbols` callers. `None` when nothing matches.
pub async fn export_neighborhood(
    pool: &PgPool,
    corpus_slug: &str,
    symbol: &str,
    depth: i64,
    max_nodes: i64,
) -> Result<Option<GraphExport>> {
    let hits = find_symbols(pool, corpus_slug, symbol, 50, None).await?;
    if hits.is_empty() {
        return Ok(None);
    }
    let root = hits[pick_show_match(&hits, symbol)].id;
    let depth = depth.clamp(0, 5);
    let max_nodes = max_nodes.clamp(1, 2000);

    // BFS over the undirected `calls` graph (so both callers and callees are
    // included), keeping each node's MIN hop distance, then take the closest
    // `max_nodes`.
    let ids: Vec<Uuid> = sqlx::query_scalar(
        r#"
        WITH RECURSIVE nbr(id, d) AS (
            SELECT $1::uuid, 0
            UNION
            SELECT CASE WHEN e.src_id = n.id THEN e.dst_id ELSE e.src_id END, n.d + 1
              FROM nbr n
              JOIN brain_vault_edges e
                ON (e.src_id = n.id OR e.dst_id = n.id) AND e.edge_type = 'calls'
             WHERE n.d < $2
        )
        SELECT id FROM (SELECT id, min(d) AS d FROM nbr GROUP BY id) q
         ORDER BY d, id
         LIMIT $3"#,
    )
    .bind(root)
    .bind(depth)
    .bind(max_nodes)
    .fetch_all(pool)
    .await?;
    if ids.is_empty() {
        return Ok(None);
    }

    let node_rows =
        sqlx::query("SELECT id, title, node_type FROM brain_vault_nodes WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(pool)
            .await?;
    let nodes = node_rows
        .into_iter()
        .map(|r| {
            let nt: String = r.get("node_type");
            let kind = nt.strip_prefix("code:").unwrap_or(&nt).to_string();
            (
                r.get::<Uuid, _>("id").to_string(),
                r.get::<String, _>("title"),
                kind,
            )
        })
        .collect();

    // Edges among the selected nodes only (so the export is self-contained).
    let edge_rows = sqlx::query(
        "SELECT src_id, dst_id, edge_type FROM brain_vault_edges
          WHERE edge_type = 'calls' AND src_id = ANY($1) AND dst_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    let edges = edge_rows
        .into_iter()
        .map(|r| {
            (
                r.get::<Uuid, _>("src_id").to_string(),
                r.get::<Uuid, _>("dst_id").to_string(),
                r.get::<String, _>("edge_type"),
            )
        })
        .collect();

    Ok(Some(GraphExport { nodes, edges }))
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

fn symbol_ref_to_hit(symbol: SymbolRef) -> SymbolHit {
    SymbolHit {
        id: symbol.id,
        qualified_name: symbol.qualified_name,
        node_type: symbol.node_type,
        file: symbol.file,
        start_line: symbol.start_line,
        fan_in: 0,
        score: None,
    }
}

fn cortex_search_document(hit: &SymbolHit) -> String {
    let location = match (&hit.file, hit.start_line) {
        (Some(file), Some(line)) => format!("{file}:{line}"),
        (Some(file), None) => file.clone(),
        (None, Some(line)) => format!("line {line}"),
        (None, None) => "unknown location".to_string(),
    };
    format!(
        "{}\n{}\n{}\nfan_in: {}\nvector_score: {}",
        hit.qualified_name,
        hit.node_type,
        location,
        hit.fan_in,
        hit.score
            .map(|s| format!("{s:.4}"))
            .unwrap_or_else(|| "n/a".to_string())
    )
}

/// Hybrid code search: semantic vector search + graph-neighborhood expansion +
/// cross-encoder reranking.
///
/// First-stage semantic search finds intent-matched symbols. One-hop direct
/// callers/callees add nearby graph context that pure vector search can miss.
/// A fleet reranker then scores the final candidate documents; if no reranker is
/// healthy, the first-stage order is returned so the tool remains usable.
pub async fn cortex_search(
    pool: &PgPool,
    corpus_slug: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SymbolHit>> {
    let limit = limit.clamp(1, 50);
    let vector_hits = find_symbols_semantic(pool, corpus_slug, query, 30, None).await?;
    let mut candidates = Vec::with_capacity(50);
    let mut seen = HashSet::new();

    for hit in vector_hits {
        if seen.insert(hit.id) {
            candidates.push(hit.clone());
        }
        if candidates.len() >= 50 {
            break;
        }

        let mut neighbors = Vec::new();
        if let Ok(mut rows) = callers(pool, corpus_slug, &hit.qualified_name, 0.0).await {
            rows.truncate(3);
            neighbors.extend(rows);
        }
        if let Ok(mut rows) = callees(pool, corpus_slug, &hit.qualified_name, 0.0).await {
            rows.truncate(3);
            neighbors.extend(rows);
        }
        for neighbor in neighbors {
            if candidates.len() >= 50 {
                break;
            }
            if seen.insert(neighbor.id) {
                candidates.push(symbol_ref_to_hit(neighbor));
            }
        }
    }

    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let docs: Vec<String> = candidates.iter().map(cortex_search_document).collect();
    match crate::embeddings::fleet_rerank(pool, query, &docs, limit).await {
        Ok(ranked) => Ok(ranked
            .into_iter()
            .filter_map(|(idx, score)| {
                candidates.get(idx).cloned().map(|mut hit| {
                    hit.score = Some(score);
                    hit
                })
            })
            .collect()),
        Err(e) => {
            tracing::warn!("cortex_search rerank failed; falling back to vector order: {e}");
            candidates.truncate(limit);
            Ok(candidates)
        }
    }
}

// ─── Symbol source retrieval (`ff cortex show`) ──────────────────────────────
//
// Collapse the agent's two-step "find a symbol → open its file and read the
// span" into one token-efficient call: resolve a name to its owning file +
// 1-based line span (V124 `start_line`/`end_line`), then return JUST that
// symbol's source slice. The Cortex-native equivalent of CRG's
// `get_review_context` — an agent gets the definition without scanning the file.

/// One resolved symbol plus its source slice, from [`show_symbol`].
#[derive(Debug, Clone)]
pub struct SymbolSource {
    pub qualified_name: String,
    pub node_type: String,
    /// Absolute path of the owning `content:file` node.
    pub file: String,
    /// 1-based inclusive line span of the symbol in `file`.
    pub start_line: i32,
    pub end_line: i32,
    pub fan_in: i64,
    /// 1-based line number of the FIRST line in `source`. Equals `start_line`
    /// when `context == 0`; with `--context N` the slice starts up to N lines
    /// earlier (clamped to 1), so the renderer numbers the gutter from here.
    pub display_start: i32,
    /// The extracted source: lines `display_start ..= end_line + context`,
    /// possibly truncated to `max_lines` — see `truncated`.
    pub source: String,
    /// True when the span exceeded `max_lines` and `source` was cut short.
    pub truncated: bool,
    /// Other qualified names that also matched the query (so the caller can hint
    /// "did you mean …" when the pick was ambiguous). Excludes the chosen symbol.
    pub other_matches: Vec<String>,
}

/// The last `::`- or `.`-separated segment of a qualified name (its leaf), used
/// for "exact leaf match" disambiguation in [`pick_show_match`]. Pure.
fn leaf_name(qualified: &str) -> &str {
    qualified
        .rsplit_once("::")
        .map(|(_, leaf)| leaf)
        .or_else(|| qualified.rsplit_once('.').map(|(_, leaf)| leaf))
        .unwrap_or(qualified)
}

/// Choose which hit `show` should display, given the fan-in-ranked `hits` and
/// the user's `query`. Preference order, most-specific first:
///   1. an exact (case-insensitive) qualified-name match,
///   2. an exact (case-insensitive) leaf-name match (highest fan-in wins, since
///      `hits` is already fan-in-desc so the first such hit is the highest),
///   3. the top hit (highest fan-in substring/semantic match).
/// Returns the index into `hits`. Pure + unit-tested. `hits` must be non-empty.
fn pick_show_match(hits: &[SymbolHit], query: &str) -> usize {
    if let Some(i) = hits
        .iter()
        .position(|h| h.qualified_name.eq_ignore_ascii_case(query))
    {
        return i;
    }
    if let Some(i) = hits
        .iter()
        .position(|h| leaf_name(&h.qualified_name).eq_ignore_ascii_case(query))
    {
        return i;
    }
    0
}

/// Widen a symbol's `[start, end]` span by `context` lines on each side for
/// `--context N` display: the leading edge clamps to line 1, the trailing edge
/// is left for the slicer to bound by file length. Returns `(display_start,
/// display_end)`; with `context == 0` it's exactly the symbol span. Pure +
/// unit-tested.
fn context_display_range(start: i32, end: i32, context: usize) -> (i32, i32) {
    let ctx = context as i32;
    let display_start = (start - ctx).max(1);
    let display_end = end.saturating_add(ctx);
    (display_start, display_end)
}

/// Extract the 1-based inclusive line range `[start, end]` from `source`,
/// capped at `max_lines` lines. Returns `(slice, truncated)`. Pure +
/// unit-tested — IO is the caller's (reads the file, then slices).
fn slice_source_lines(source: &str, start: i32, end: i32, max_lines: usize) -> (String, bool) {
    let start = start.max(1) as usize;
    let end = end.max(start as i32) as usize;
    let want = end - start + 1;
    let take = want.min(max_lines.max(1));
    let truncated = want > take;
    let slice: String = source
        .lines()
        .skip(start - 1)
        .take(take)
        .collect::<Vec<_>>()
        .join("\n");
    (slice, truncated)
}

/// Resolve `query` to a single symbol in `corpus_slug` and return its source
/// slice. Reuses [`find_symbols`] for resolution (so `--kind` + escaping +
/// fan-in ranking all apply), picks the best match via [`pick_show_match`],
/// then reads the owning file and slices the symbol's line span.
///
/// Returns `Ok(None)` when nothing matched. Errors (loudly) when the matched
/// symbol has no source span (an extern/import placeholder or a pre-V124 node —
/// re-`ff cortex index`) or when its owning file can't be read on this host
/// (the corpus was indexed on a different checkout — same constraint as
/// `ff cortex review`).
pub async fn show_symbol(
    pool: &PgPool,
    corpus_slug: &str,
    query: &str,
    kind: Option<&str>,
    max_lines: usize,
    context: usize,
) -> Result<Option<SymbolSource>> {
    // Resolve against a generous candidate set so leaf/exact disambiguation has
    // material to work with, then narrow to one.
    let hits = find_symbols(pool, corpus_slug, query, 50, kind).await?;
    if hits.is_empty() {
        return Ok(None);
    }
    let idx = pick_show_match(&hits, query);
    let chosen = &hits[idx];

    let Some(file) = chosen.file.clone() else {
        anyhow::bail!(
            "symbol '{}' matched but has no owning file (extern/import placeholder) — \
             nothing to show",
            chosen.qualified_name
        );
    };
    let Some(start_line) = chosen.start_line else {
        anyhow::bail!(
            "symbol '{}' has no source span (pre-V124 index) — re-run `ff cortex index`",
            chosen.qualified_name
        );
    };
    // start_line lives on the hit; end_line isn't on SymbolHit, so fetch it.
    let end_line: Option<i32> =
        sqlx::query_scalar("SELECT end_line FROM brain_vault_nodes WHERE id = $1")
            .bind(chosen.id)
            .fetch_optional(pool)
            .await?
            .flatten();
    let end_line = end_line.unwrap_or(start_line).max(start_line);

    let source_text = std::fs::read_to_string(&file).map_err(|e| {
        anyhow::anyhow!(
            "read {file}: {e} — the file isn't on this host (corpus indexed elsewhere?); \
             `ff cortex show` needs the indexed checkout present, like `ff cortex review`"
        )
    })?;
    // With `--context N`, widen the slice by N lines on each side (the leading
    // edge clamps to line 1; the trailing edge is bounded by the file length via
    // `slice_source_lines`' `take`). `display_start` is what the renderer numbers
    // the gutter from; the symbol's own span (start_line..=end_line) is unchanged.
    let (display_start, display_end) = context_display_range(start_line, end_line, context);
    let (source, truncated) =
        slice_source_lines(&source_text, display_start, display_end, max_lines);

    let other_matches: Vec<String> = hits
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(_, h)| h.qualified_name.clone())
        .take(10)
        .collect();

    Ok(Some(SymbolSource {
        qualified_name: chosen.qualified_name.clone(),
        node_type: chosen.node_type.clone(),
        file,
        start_line,
        end_line,
        fan_in: chosen.fan_in,
        display_start,
        source,
        truncated,
        other_matches,
    }))
}

// ─── Community explanation (`ff cortex explain`) ─────────────────────────────
//
// The consumer side of cortex roadmap #4. `ff cortex summarize` writes one
// natural-language summary per code-graph community (the Leiden / connected-
// component cluster a symbol lives in) onto `brain_communities`, but nothing
// read those summaries back — they were write-only. `explain_community` resolves
// a symbol (or any name) to the community it belongs to and returns that
// community's stored summary plus its highest-fan-in members: the GraphRAG
// "what is this subsystem responsible for?" answer, in one token-cheap call,
// so an agent can orient on a cluster without reading every file in it.

/// One member symbol of a community, for [`CommunityExplanation`].
#[derive(Debug, Clone)]
pub struct CommunityMember {
    pub qualified_name: String,
    pub node_type: String,
    pub fan_in: i64,
}

/// One ancestor in the GraphRAG community hierarchy above a symbol's finest
/// community — a progressively-larger SUBSYSTEM (level 1, 2, …) with its own
/// map-reduce summary. Used by [`CommunityExplanation::subsystem_chain`].
#[derive(Debug, Clone)]
pub struct CommunitySubsystem {
    /// Hierarchy level (1 = the immediate parent subsystem, larger = coarser).
    pub level: i32,
    /// Total code symbols this subsystem spans.
    pub member_count: i64,
    /// The subsystem's map-reduce summary (`None` until it's been summarized).
    pub summary: Option<String>,
    /// The subsystem's representative ("god") symbol, if any.
    pub god_symbol: Option<String>,
}

/// The community a symbol belongs to + its stored summary, from
/// [`explain_community`].
#[derive(Debug, Clone)]
pub struct CommunityExplanation {
    /// The symbol the query actually resolved to (so the caller can confirm the
    /// match before trusting the rest).
    pub resolved_symbol: String,
    pub resolved_node_type: String,
    /// The global community id, or `None` when the graph hasn't been community-
    /// detected yet (the fix is `ff cortex embed`).
    pub community_id: Option<i32>,
    /// Whole-community member count from the registry (spans corpora); falls back
    /// to the in-corpus members actually pulled when no registry row exists.
    pub member_count: i64,
    /// The fleet-LLM summary, present once `ff cortex summarize` has covered this
    /// community. `None` means the cluster exists but hasn't been summarized.
    pub summary: Option<String>,
    pub summary_model: Option<String>,
    /// The representative (highest-degree) member — the community's "god node".
    pub god_symbol: Option<String>,
    /// Highest-fan-in members in this corpus (the cluster's call-graph surface),
    /// capped by `member_limit`.
    pub members: Vec<CommunityMember>,
    /// The GraphRAG subsystem hierarchy ABOVE this community: walking
    /// `parent_member_hash` up the tree, each progressively-larger subsystem with
    /// its own summary (immediate parent first). Empty when the community is
    /// top-level or the hierarchy hasn't been built. Answers "what larger
    /// subsystem is this part of, and what does that subsystem do?".
    pub subsystem_chain: Vec<CommunitySubsystem>,
}

/// Resolve `query` to a code symbol, then return the community it belongs to plus
/// that community's stored summary and top members. Resolution mirrors
/// [`show_symbol`] (exact qualified → exact leaf, highest fan-in → top hit).
/// Returns `Ok(None)` only when nothing matched the query at all; a matched
/// symbol with no community / no summary yet returns `Ok(Some(..))` with the
/// relevant field `None` so the caller can guide the user to the right command.
pub async fn explain_community(
    pool: &PgPool,
    corpus_slug: &str,
    query: &str,
    kind: Option<&str>,
    member_limit: i64,
) -> Result<Option<CommunityExplanation>> {
    let hits = find_symbols(pool, corpus_slug, query, 50, kind).await?;
    if hits.is_empty() {
        return Ok(None);
    }
    let chosen = &hits[pick_show_match(&hits, query)];

    // The chosen symbol's CODE community (assigned by `detect_code_communities`
    // — label propagation over the `calls` subgraph, not the brain-KG
    // connected-components `community_id`).
    let community_id: Option<i32> =
        sqlx::query_scalar("SELECT code_community_id FROM brain_vault_nodes WHERE id = $1")
            .bind(chosen.id)
            .fetch_optional(pool)
            .await?
            .flatten();

    let Some(cid) = community_id else {
        return Ok(Some(CommunityExplanation {
            resolved_symbol: chosen.qualified_name.clone(),
            resolved_node_type: chosen.node_type.clone(),
            community_id: None,
            member_count: 0,
            summary: None,
            summary_model: None,
            god_symbol: None,
            members: Vec::new(),
            subsystem_chain: Vec::new(),
        }));
    };

    // The registry row, located via the god node (which carries the same
    // code_community_id). Detection is global, so we match on code_community_id,
    // not corpus.
    let reg = sqlx::query(
        r#"SELECT bc.member_hash, bc.summary, bc.summary_model, bc.member_count, gn.title AS god_title
             FROM brain_code_communities bc
             JOIN brain_vault_nodes gn ON gn.id = bc.god_node_id
            WHERE gn.code_community_id = $1 AND gn.valid_until IS NULL
            LIMIT 1"#,
    )
    .bind(cid)
    .fetch_optional(pool)
    .await?;

    let (member_hash, summary, summary_model, reg_member_count, god_symbol) = match reg {
        Some(r) => (
            Some(r.get::<String, _>("member_hash")),
            r.get::<Option<String>, _>("summary"),
            r.get::<Option<String>, _>("summary_model"),
            r.get::<i32, _>("member_count") as i64,
            Some(r.get::<String, _>("god_title")),
        ),
        None => (None, None, None, 0i64, None),
    };

    // GraphRAG subsystem hierarchy: walk parent_member_hash up the tree from this
    // (finest) community, collecting each progressively-larger subsystem + its
    // map-reduce summary (immediate parent first).
    let subsystem_chain: Vec<CommunitySubsystem> = if let Some(ref mh) = member_hash {
        let rows = sqlx::query(
            r#"
            WITH RECURSIVE chain AS (
                SELECT member_hash, parent_member_hash, summary, member_count, level,
                       god_node_id, 0 AS depth
                  FROM brain_code_communities
                 WHERE member_hash = $1
                UNION ALL
                SELECT p.member_hash, p.parent_member_hash, p.summary, p.member_count,
                       p.level, p.god_node_id, c.depth + 1
                  FROM brain_code_communities p
                  JOIN chain c ON p.member_hash = c.parent_member_hash
                 WHERE c.depth < 16
            )
            SELECT c.level, c.member_count, c.summary,
                   (SELECT gn.title FROM brain_vault_nodes gn WHERE gn.id = c.god_node_id) AS god_title
              FROM chain c
             WHERE c.depth > 0
             ORDER BY c.level"#,
        )
        .bind(mh)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|r| CommunitySubsystem {
                level: r.get::<i32, _>("level"),
                member_count: r.get::<i32, _>("member_count") as i64,
                summary: r.get::<Option<String>, _>("summary"),
                god_symbol: r.get::<Option<String>, _>("god_title"),
            })
            .collect()
    } else {
        Vec::new()
    };

    // Top members of this community within the queried corpus, by call-graph
    // fan-in (the same fan_in subquery `find_symbols` uses).
    let member_limit = member_limit.clamp(1, 200);
    let member_rows = sqlx::query(
        r#"SELECT n.title, n.node_type,
                  (SELECT count(*) FROM brain_vault_edges e
                    WHERE e.edge_type = 'calls' AND e.dst_id = n.id) AS fan_in
             FROM brain_vault_nodes n
            WHERE n.code_community_id = $1
              AND n.valid_until IS NULL
              AND n.node_type LIKE 'code:%'
              AND n.node_type <> 'code:extern'
              AND n.project = $2
            ORDER BY fan_in DESC, n.title COLLATE "C"
            LIMIT $3"#,
    )
    .bind(cid)
    .bind(corpus_slug)
    .bind(member_limit)
    .fetch_all(pool)
    .await?;

    let members: Vec<CommunityMember> = member_rows
        .into_iter()
        .map(|r| CommunityMember {
            qualified_name: r.get("title"),
            node_type: r.get("node_type"),
            fan_in: r.get("fan_in"),
        })
        .collect();

    // Registry count covers the whole (possibly cross-corpus) community; fall
    // back to what we pulled when the registry has no row yet.
    let member_count = if reg_member_count > 0 {
        reg_member_count
    } else {
        members.len() as i64
    };

    Ok(Some(CommunityExplanation {
        resolved_symbol: chosen.qualified_name.clone(),
        resolved_node_type: chosen.node_type.clone(),
        community_id: Some(cid),
        member_count,
        summary,
        summary_model,
        god_symbol,
        members,
        subsystem_chain,
    }))
}

// ─── File outline (`ff cortex outline`) ──────────────────────────────────────
//
// A file-level table of contents: every code symbol a file defines, in source
// order, with its kind / line span / fan-in. Lets an agent orient in an unknown
// file from the graph (one call) instead of reading the whole file and eyeballing
// its structure. Reuses the V124 spans + the `contains`-edge walk (the inverse of
// `owning_files`: descend from the file node to its symbols).

/// One symbol in a [`FileOutline`] — the inverse of a [`SymbolHit`] (we already
/// know the file; here we list what it defines).
#[derive(Debug, Clone)]
pub struct OutlineEntry {
    pub qualified_name: String,
    pub node_type: String,
    /// 1-based span (None for placeholders / pre-V124 nodes).
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    pub fan_in: i64,
}

/// The symbols a single file defines, from [`outline_file`].
#[derive(Debug, Clone)]
pub struct FileOutline {
    /// The resolved absolute path of the matched `content:file` node.
    pub file: String,
    /// Symbols defined in the file, in source order (start_line asc).
    pub symbols: Vec<OutlineEntry>,
}

/// Outcome of matching a file argument against the corpus's `content:file` paths.
/// Pure (no Uuids) so it can be unit-tested against the candidate path list.
#[derive(Debug, PartialEq)]
pub enum FileChoice {
    /// No `content:file` node matched.
    None,
    /// Exactly one match (its index into the candidate list).
    Unique(usize),
    /// Multiple suffix matches and no exact one — the caller must disambiguate.
    Ambiguous,
}

/// The `ILIKE`-suffix pattern for resolving a file argument: `%/<escaped arg>` so
/// `cortex.rs` matches `.../src/cortex.rs` but NOT `mycortex.rs` (the `/` forces a
/// path-component boundary). Wildcards in the arg are escaped (paired with
/// `ESCAPE '\'`). Pure + unit-tested.
pub fn file_match_pattern(file_arg: &str) -> String {
    format!("%/{}", escape_like(file_arg))
}

/// Choose which `content:file` path the argument resolves to, given the candidate
/// `paths` (exact match OR suffix match, as fetched by [`outline_file`]). An exact
/// full-path match always wins; otherwise a single candidate is taken; more than
/// one suffix match with no exact hit is [`FileChoice::Ambiguous`]. Pure +
/// unit-tested.
pub fn choose_file_match(paths: &[&str], file_arg: &str) -> FileChoice {
    if paths.is_empty() {
        return FileChoice::None;
    }
    if let Some(i) = paths.iter().position(|p| *p == file_arg) {
        return FileChoice::Unique(i);
    }
    if paths.len() == 1 {
        return FileChoice::Unique(0);
    }
    FileChoice::Ambiguous
}

/// Resolve `file_arg` to a single `content:file` node (its id + absolute path) in
/// `corpus_slug`. Matches an exact path or a `%/<arg>` suffix; an exact match wins,
/// a lone suffix match is taken, and multiple suffix matches with no exact hit
/// bail loudly with the candidate list (so the caller passes more of the path).
async fn resolve_file_node(
    pool: &PgPool,
    corpus_slug: &str,
    file_arg: &str,
) -> Result<Option<(Uuid, String)>> {
    let suffix = file_match_pattern(file_arg);
    let rows = sqlx::query(
        r#"SELECT id, path FROM brain_vault_nodes
            WHERE project = $1 AND node_type = 'content:file'
              AND (path = $2 OR path LIKE $3 ESCAPE '\')
            ORDER BY length(path) ASC, path COLLATE "C""#,
    )
    .bind(corpus_slug)
    .bind(file_arg)
    .bind(&suffix)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let paths: Vec<String> = rows.iter().map(|r| r.get::<String, _>("path")).collect();
    let path_refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    match choose_file_match(&path_refs, file_arg) {
        FileChoice::None => Ok(None),
        FileChoice::Unique(i) => Ok(Some((rows[i].get("id"), paths[i].clone()))),
        FileChoice::Ambiguous => {
            let shown = paths
                .iter()
                .take(10)
                .map(|p| format!("  {p}"))
                .collect::<Vec<_>>()
                .join("\n");
            let more = if paths.len() > 10 {
                format!("\n  … and {} more", paths.len() - 10)
            } else {
                String::new()
            };
            anyhow::bail!(
                "'{file_arg}' matches {} files in corpus '{corpus_slug}' — pass more of the \
                 path to disambiguate:\n{shown}{more}",
                paths.len()
            )
        }
    }
}

/// List every `code:*` symbol a file defines, in source order — a file-level
/// table of contents. Resolves `file_arg` to a `content:file` node (exact path or
/// `%/<arg>` suffix), then descends `contains` edges to collect its symbols
/// (including nested ones: `file → impl/mod → fn`), each with kind / span / fan-in.
/// `kind` optionally narrows to a node-type class (see [`kind_filter_types`]).
///
/// Returns `Ok(None)` when no file matched. Errors loudly on an ambiguous match
/// (multiple suffix hits) so the caller disambiguates. No file IO — purely a graph
/// query (unlike `show`, which reads the source), so it works even when the
/// indexed checkout isn't present on this host.
pub async fn outline_file(
    pool: &PgPool,
    corpus_slug: &str,
    file_arg: &str,
    kind: Option<&str>,
) -> Result<Option<FileOutline>> {
    let Some((file_id, path)) = resolve_file_node(pool, corpus_slug, file_arg).await? else {
        return Ok(None);
    };
    let kind_types = resolve_kind_filter(kind)?;
    let rows = sqlx::query(
        r#"WITH RECURSIVE down AS (
                SELECT e.dst_id AS node
                  FROM brain_vault_edges e
                 WHERE e.edge_type = 'contains' AND e.src_id = $1
                UNION
                SELECT e.dst_id
                  FROM brain_vault_edges e
                  JOIN down ON e.src_id = down.node
                 WHERE e.edge_type = 'contains'
            )
            SELECT n.id, n.title, n.node_type, n.start_line, n.end_line,
                   (SELECT count(*) FROM brain_vault_edges ce
                     WHERE ce.edge_type = 'calls' AND ce.dst_id = n.id) AS fan_in
              FROM down JOIN brain_vault_nodes n ON n.id = down.node
             WHERE n.node_type LIKE 'code:%'
               AND ($2::text[] IS NULL OR n.node_type = ANY($2))
             ORDER BY n.start_line ASC NULLS LAST, n.title COLLATE "C""#,
    )
    .bind(file_id)
    .bind(kind_types.as_deref())
    .fetch_all(pool)
    .await?;

    let symbols = rows
        .into_iter()
        .map(|r| OutlineEntry {
            qualified_name: r.get("title"),
            node_type: r.get("node_type"),
            start_line: r.get("start_line"),
            end_line: r.get("end_line"),
            fan_in: r.get("fan_in"),
        })
        .collect();
    Ok(Some(FileOutline {
        file: path,
        symbols,
    }))
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
                file: None,
                start_line: r.get("start_line"),
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
                // Review wants the FULL blast radius — no confidence filter (0.0).
                let direct = callers_of_ids(pool, &[s.id], 0.0).await?;
                let blast = impact_of_ids(pool, &[s.id], depth, 0.0).await?;
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
    generation: i64,
    confidence: f32,
    provenance: &str,
) -> Result<Uuid> {
    // content_hash is NOT NULL; use the path (synthetic + unique) as a stable hash.
    // start_line/end_line are 1-based source spans (V124) — set for real symbol
    // nodes (so `review` can do hunk-level filtering), NULL for import/extern
    // placeholders. On conflict we refresh them so an incremental reindex (which
    // KEEPS the stable node and re-upserts it) tracks the symbol as it moves.
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO brain_vault_nodes
               (path, title, node_type, project, content_hash, start_line, end_line,
                generation, confidence, provenance)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           ON CONFLICT (path) DO UPDATE
             SET title = EXCLUDED.title, node_type = EXCLUDED.node_type,
                 project = EXCLUDED.project, valid_until = NULL, updated_at = NOW(),
                 start_line = EXCLUDED.start_line, end_line = EXCLUDED.end_line,
                 generation = EXCLUDED.generation,
                 confidence = GREATEST(brain_vault_nodes.confidence, EXCLUDED.confidence),
                 provenance = EXCLUDED.provenance
           RETURNING id"#,
    )
    .bind(path)
    .bind(title)
    .bind(node_type)
    .bind(project)
    .bind(path)
    .bind(start_line)
    .bind(end_line)
    .bind(generation)
    .bind(confidence)
    .bind(provenance)
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
async fn add_edge(
    pool: &PgPool,
    src: Uuid,
    dst: Uuid,
    edge_type: &str,
    generation: i64,
) -> Result<bool> {
    add_edge_with_metadata(
        pool,
        src,
        dst,
        edge_type,
        1.0,
        "ast",
        Some("EXTRACTED"),
        None,
        generation,
    )
    .await
}

async fn add_edge_with_metadata(
    pool: &PgPool,
    src: Uuid,
    dst: Uuid,
    edge_type: &str,
    confidence: f32,
    provenance: &str,
    method: Option<&str>,
    evidence: Option<&serde_json::Value>,
    generation: i64,
) -> Result<bool> {
    if src == dst && edge_type == "calls" {
        // skip trivial self-loops produced by recursion noise? keep recursion
        // edges — they are real. Only the parse-error false-self case is avoided
        // upstream via ERROR-node descent, so allow self here.
    }
    let inserted: bool = sqlx::query_scalar(
        r#"INSERT INTO brain_vault_edges
               (src_id, dst_id, edge_type, provenance, confidence, method, evidence, generation)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           ON CONFLICT (src_id, dst_id, edge_type) DO UPDATE
             SET confidence = GREATEST(brain_vault_edges.confidence, EXCLUDED.confidence),
                 provenance = EXCLUDED.provenance,
                 method = COALESCE(EXCLUDED.method, brain_vault_edges.method),
                 evidence = COALESCE(EXCLUDED.evidence, brain_vault_edges.evidence),
                 generation = EXCLUDED.generation
           RETURNING (xmax = 0) AS inserted"#,
    )
    .bind(src)
    .bind(dst)
    .bind(edge_type)
    .bind(provenance)
    .bind(confidence)
    .bind(method)
    .bind(evidence)
    .bind(generation)
    .fetch_one(pool)
    .await?;
    Ok(inserted)
}

/// Write a `calls` edge with a resolution-confidence tier (roadmap #5 — provenance
/// tiers, graphify's one structural advantage). `confidence` encodes how much to
/// trust that this edge points at the *correct* callee symbol:
///   - `1.0` EXTRACTED  — the primary resolver landed the call on a real internal
///     symbol it explicitly named (alias/import/crate/self/super or a direct
///     internal match). High confidence.
///   - `0.6` INFERRED   — only a heuristic redirect (glob / impl-method / Self::
///     method / facade `pub use`) chose this internal symbol among same-leaf
///     candidates. Real target, but guessed.
///   - `0.3` UNRESOLVED — the callee is a `code:extern` placeholder (genuine
///     std/3rd-party OR a mis-resolution the resolver kept as written).
/// Distinct from `add_edge` only in the `ON CONFLICT` clause: two call sites from
/// the same caller to the same callee may resolve at different tiers, so we keep
/// the MAX (the highest-trust resolution wins) rather than first-write-wins. The
/// dst node_type already separates internal vs extern; `confidence` adds the
/// EXTRACTED-vs-INFERRED split *within* the internal tier that downstream
/// consumers can filter on.
async fn add_call_edge(
    pool: &PgPool,
    src: Uuid,
    dst: Uuid,
    confidence: f32,
    generation: i64,
) -> Result<()> {
    let method = if confidence >= 1.0 {
        "EXTRACTED"
    } else if confidence >= 0.6 {
        "INFERRED"
    } else {
        "HEURISTIC"
    };
    sqlx::query(
        r#"INSERT INTO brain_vault_edges
               (src_id, dst_id, edge_type, provenance, confidence, method, generation)
           VALUES ($1, $2, 'calls', 'cortex', $3, $4, $5)
           ON CONFLICT (src_id, dst_id, edge_type)
           DO UPDATE SET confidence = GREATEST(brain_vault_edges.confidence, EXCLUDED.confidence),
                         provenance = EXCLUDED.provenance,
                         method = CASE
                             WHEN EXCLUDED.confidence >= brain_vault_edges.confidence
                             THEN EXCLUDED.method
                             ELSE brain_vault_edges.method
                         END,
                         generation = EXCLUDED.generation"#,
    )
    .bind(src)
    .bind(dst)
    .bind(confidence)
    .bind(method)
    .bind(generation)
    .execute(pool)
    .await?;
    Ok(())
}

/// Map a call-resolution outcome to its confidence tier (roadmap #5 — provenance
/// tiers). `primary_hit` = the primary resolver (`resolve_call`) already named a
/// known internal fn; `resolved_internal` = the final callee is a real internal
/// symbol (not a `code:extern` placeholder).
///   - `1.0` EXTRACTED  — primary resolver landed on a real internal symbol.
///   - `0.6` INFERRED   — only a heuristic redirect landed it on an internal symbol.
///   - `0.3` UNRESOLVED — callee is an extern placeholder.
/// `(true, false)` is unreachable in practice (a primary hit is always internal),
/// but is treated as UNRESOLVED for totality — the written edge is what matters.
fn call_confidence(primary_hit: bool, resolved_internal: bool) -> f32 {
    match (primary_hit, resolved_internal) {
        (_, false) => 0.3,
        (true, true) => 1.0,
        (false, true) => 0.6,
    }
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
        glob_imports: Vec::new(),
        reexports: Vec::new(),
        glob_reexports: Vec::new(),
        wildcard_imports: Vec::new(),
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
                // A `pub`/`pub(crate)` re-export aliases the imported items under
                // the current module path; record them for the facade redirect.
                let mut vc = child.walk();
                let is_pub = child
                    .children(&mut vc)
                    .any(|c| c.kind() == "visibility_modifier");
                collect_use(&child, bytes, mod_path, crate_name, is_pub, fp);
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

/// Bare Rust prelude variant constructors that are NOT useful call-graph edges.
/// `Ok(x)`, `Err(e)`, `Some(v)`, `None` parse as `call_expression`s whose function
/// is a bare `identifier`, so the resolver fabricates a `<caller_module>::Ok`
/// `code:extern` and hangs a `calls` edge on it. These prelude constructors appear
/// in essentially every Rust fn (every `Result`/`Option` return), so they tell you
/// nothing structural while polluting the extern set (~19% of forge-fleet's externs
/// were one of these) and inflating fan-in noise — `ff cortex find Ok` returned a
/// phantom `ff_db::queries::Ok` as its top hit. They can never resolve to an
/// internal symbol, so dropping them at collection is lossless for the real graph.
/// Scoped forms (`Result::Ok`, `Option::Some`) are left alone — they're rare and
/// already resolve as externs under their real type path, not the caller's module.
fn is_rust_prelude_ctor(raw: &str) -> bool {
    matches!(raw, "Ok" | "Err" | "Some" | "None")
}

/// Collect call sites in a function body (call_expression with a path/identifier
/// function). We record the raw path text + byte offset for later attribution.
fn collect_calls(node: &Node, bytes: &[u8], fp: &mut FileParse) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "call_expression" {
            if let Some(func) = child.child_by_field_name("function") {
                if let Some(raw) = call_target_path(&func, bytes) {
                    if !is_rust_prelude_ctor(&raw) {
                        fp.calls.push(CallSite {
                            raw_path: raw,
                            at: child.start_byte(),
                        });
                    }
                }
            }
        }
        // Macro-body calls: tree-sitter parses a macro invocation's arguments as
        // an opaque `token_tree`, so `assert_eq!(foo(), …)` yields NO
        // call_expression — the single biggest Rust call-graph blind spot, since
        // unit tests assert through macros (`ff cortex tests` read those fns as
        // uncovered). Recover them by spotting the `ident( … )` shape inside a
        // token_tree: an `identifier`/`scoped_identifier` whose immediate next
        // sibling is a `(`-delimited token_tree. The macro's own name is excluded
        // for free — the `!` token sits between it and its token_tree — and method
        // calls (`x.bar()`, a preceding `.`) are skipped to match the
        // call_expression path, which also drops field-expression methods.
        else if matches!(child.kind(), "identifier" | "scoped_identifier") {
            if let Some(next) = child.next_sibling() {
                let is_paren_tt =
                    next.kind() == "token_tree" && bytes.get(next.start_byte()) == Some(&b'(');
                let is_method = child.prev_sibling().map(|p| p.kind()) == Some(".");
                if is_paren_tt && !is_method {
                    if let Some(raw) = macro_call_path(&child, bytes) {
                        if !is_rust_prelude_ctor(&raw) {
                            fp.calls.push(CallSite {
                                raw_path: raw,
                                at: child.start_byte(),
                            });
                        }
                    }
                }
            }
        }
        // Recurse — calls can be nested arbitrarily, but NOT into:
        //  - nested function_items (separate symbols handled by walk()), or
        //  - attribute items. Attribute arguments parse as `identifier (token_tree)`
        //    — the same shape the macro-body recovery above keys on — so
        //    `#[derive(Debug)]`, `#[cfg(test)]`, `#[serde(rename = "x")]`,
        //    `#[arg(long)]` etc. would be recorded as phantom calls
        //    (`<caller_module>::derive`, `::cfg`, `::serde`…). Attributes are
        //    metadata, never call-graph edges, so descending into them is pure noise.
        if !matches!(
            child.kind(),
            "function_item" | "attribute_item" | "inner_attribute_item"
        ) {
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

/// Reconstruct a call path from inside a macro `token_tree`. A macro body is an
/// opaque token sequence — not parsed as Rust grammar — so a scoped associated
/// call `Type::method(args)` is lexed as the flat tokens `Type`, `::`, `method`,
/// `(...)` rather than a single `scoped_identifier`. The macro-recovery heuristic
/// matches the trailing `method` identifier (its next sibling is the `(`-delimited
/// token_tree), so without this it would record a bare `method` and fabricate
/// `<caller_module>::method` — splitting the real method's fan-in onto a per-test
/// phantom (`ff cortex doctor` flagged `compaction::tests::{system,user,…}` for the
/// `ToolChatMessage::system(…)` calls in `vec![…]`). Walk back over leading
/// `:: <ident>` pairs from `tail` to rebuild the full `Type::method` path; the
/// later impl-method redirect then flattens it onto the real method. Returns the
/// tail's own text when it has no `::` prefix (a genuine bare call).
fn macro_call_path(tail: &Node, bytes: &[u8]) -> Option<String> {
    let mut segs = vec![node_text(tail, bytes)?];
    let mut cur = tail.prev_sibling();
    while let Some(sep) = cur {
        if sep.kind() != "::" {
            break;
        }
        let Some(ident) = sep.prev_sibling() else {
            break;
        };
        // `crate`/`self`/`super` are path ROOTS, lexed as their own keyword nodes
        // inside a macro token_tree (not `identifier`). Without them a macro-body
        // `crate::foo(…)` / `super::foo(…)` call lost its head and collapsed to a
        // bare `foo`, fabricating `<caller_module>::foo` — `ff cortex doctor`
        // flagged `ff_brain::community_summary::detect_code_communities` for the
        // `crate::detect_code_communities(…)` call inside a `tokio::select!` arm.
        if !matches!(
            ident.kind(),
            "identifier" | "scoped_identifier" | "crate" | "self" | "super"
        ) {
            break;
        }
        segs.push(node_text(&ident, bytes)?);
        // A `scoped_identifier` already carries the whole prefix path text; a
        // `crate`/`self`/`super` keyword is the path root, so stop walking back.
        if matches!(
            ident.kind(),
            "scoped_identifier" | "crate" | "self" | "super"
        ) {
            break;
        }
        cur = ident.prev_sibling();
    }
    segs.reverse();
    Some(segs.join("::"))
}

/// Collect a `use` declaration into use_targets + alias_map. Handles
/// `a::b::c`, `a::b as c`, `a::{b, c}`, and `a::{self, b}`.
fn collect_use(
    node: &Node,
    bytes: &[u8],
    mod_path: &str,
    crate_name: &str,
    is_pub: bool,
    fp: &mut FileParse,
) {
    // `pub use` re-exports the items under the current module path (the facade);
    // a private `use` is not visible to other modules, so it never re-exports.
    let reexport_base = is_pub.then_some(mod_path);
    // The argument child holds the tree (scoped_identifier / use_list / use_as_clause).
    if let Some(arg) = node.child_by_field_name("argument") {
        expand_use(&arg, bytes, "", crate_name, reexport_base, fp);
    } else {
        // Fallback: some grammars expose children directly.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "scoped_identifier" | "use_list" | "use_as_clause" | "identifier"
                | "scoped_use_list" | "use_wildcard" => {
                    expand_use(&child, bytes, "", crate_name, reexport_base, fp);
                }
                _ => {}
            }
        }
    }
}

/// Recursively expand a use-tree node under prefix `pfx` (already normalized).
/// `reexport_base` is `Some(module)` for a `pub use` (the facade module each
/// imported leaf becomes visible under), `None` for a private `use`.
fn expand_use(
    node: &Node,
    bytes: &[u8],
    pfx: &str,
    crate_name: &str,
    reexport_base: Option<&str>,
    fp: &mut FileParse,
) {
    match node.kind() {
        "identifier" => {
            if let Some(name) = node_text(node, bytes) {
                let full = norm_crate(&join(pfx, &name), crate_name);
                register_use(&full, &name, fp);
                record_reexport(reexport_base, &name, &full, crate_name, fp);
            }
        }
        "scoped_identifier" => {
            // path::name — gather full text, register leaf as alias.
            if let Some(full_raw) = node_text(node, bytes) {
                let full = norm_crate(&prefixed(pfx, &full_raw), crate_name);
                let leaf = full.rsplit("::").next().unwrap_or(&full).to_string();
                register_use(&full, &leaf, fp);
                record_reexport(reexport_base, &leaf, &full, crate_name, fp);
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
                record_reexport(reexport_base, &alias, &full, crate_name, fp);
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
                expand_use(&list, bytes, &new_pfx, crate_name, reexport_base, fp);
            } else {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "use_list" {
                        expand_use(&child, bytes, &new_pfx, crate_name, reexport_base, fp);
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
                            record_reexport(reexport_base, &leaf, pfx, crate_name, fp);
                        }
                    }
                    _ => expand_use(&child, bytes, pfx, crate_name, reexport_base, fp),
                }
            }
        }
        "self" => {
            if !pfx.is_empty() {
                let leaf = pfx.rsplit("::").next().unwrap_or(pfx).to_string();
                register_use(pfx, &leaf, fp);
                record_reexport(reexport_base, &leaf, pfx, crate_name, fp);
            }
        }
        "use_wildcard" => {
            // `use a::b::*` — register the prefix as a glob source (no alias leaf).
            // Record it in `glob_imports` so a bare call brought into scope by the
            // glob can be redirected to the real symbol at resolve time.
            if let Some(t) = node_text(node, bytes) {
                let t = t.trim_end_matches("::*").to_string();
                let full = norm_crate(&prefixed(pfx, &t), crate_name);
                fp.use_targets.push(full.clone());
                // A `pub use m::*;` re-exports every item of `m` at the module
                // where the `use` appears (the facade base) — record it so a call
                // through the facade path (`ff_db::pg_get_current_leader` for
                // `ff_db::leader_state::pg_get_current_leader`) can be redirected at
                // resolve time. The glob target is made absolute relative to the
                // re-exporting module (`pub use leader_state::*;` in `ff_db` →
                // `ff_db::leader_state`), the same way named re-exports are. Private
                // globs (`reexport_base` None) only bring names into local scope —
                // already handled by `glob_imports`.
                if let Some(base) = reexport_base {
                    let abs_target = abs_reexport_target(&full, base, crate_name);
                    fp.glob_reexports.push((base.to_string(), abs_target));
                }
                fp.glob_imports.push(full);
            }
        }
        _ => {
            // Unknown wrapper: descend.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                expand_use(&child, bytes, pfx, crate_name, reexport_base, fp);
            }
        }
    }
}

fn register_use(full: &str, leaf: &str, fp: &mut FileParse) {
    fp.use_targets.push(full.to_string());
    fp.alias_map.insert(leaf.to_string(), full.to_string());
}

/// Record a `pub use` re-export: the imported `leaf` becomes visible at
/// `<base>::<leaf>` (the facade) aliasing the real target. No-op for private
/// `use` (`base` is `None`) and for re-exports of a same-module name (facade ==
/// target, nothing to redirect). `target` is resolved absolute so the redirect
/// can match against the corpus's internal symbol set.
fn record_reexport(
    base: Option<&str>,
    leaf: &str,
    target: &str,
    crate_name: &str,
    fp: &mut FileParse,
) {
    let Some(base) = base else { return };
    let facade = join(base, leaf);
    let abs = abs_reexport_target(target, base, crate_name);
    if facade != abs {
        fp.reexports.push((facade, abs));
    }
}

/// Resolve a `pub use` target to an absolute path. `crate::` is already rewritten
/// (`norm_crate`); `super::`/`self::` are relative to the re-exporting module; a
/// path already headed by this crate or a known external crate is absolute; any
/// other bare path names a submodule of the current module.
fn abs_reexport_target(target: &str, mod_path: &str, crate_name: &str) -> String {
    let first = target.split("::").next().unwrap_or(target);
    if first == "super" || first == "self" {
        resolve_glob_prefix(target, mod_path).unwrap_or_else(|| target.to_string())
    } else if first == crate_name || looks_external(first) {
        target.to_string()
    } else {
        join(mod_path, target)
    }
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
        glob_imports: Vec::new(),
        reexports: Vec::new(),
        glob_reexports: Vec::new(),
        wildcard_imports: Vec::new(),
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
        glob_imports: Vec::new(),
        reexports: Vec::new(),
        glob_reexports: Vec::new(),
        wildcard_imports: Vec::new(),
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
        // `import a.b.*;` (package) / `import static a.b.C.*;` (static members) —
        // record the base, no leaf alias to bind. Also stash the base in
        // `wildcard_imports` so a bare call to a wildcard-imported internal member
        // can be redirected at resolve time (the non-Rust analogue of a glob `use`)
        // instead of fabricating an extern under the caller's own path.
        fp.use_targets.push(full.clone());
        fp.wildcard_imports.push(full);
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
        glob_imports: Vec::new(),
        reexports: Vec::new(),
        glob_reexports: Vec::new(),
        wildcard_imports: Vec::new(),
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
                // `from m import *` — record the base module and stash it as a
                // wildcard import so a bare call to one of its internal members can
                // be redirected to `m::<leaf>` at resolve time, the same as Java
                // wildcards, instead of fabricating `<caller_module>::<leaf>`.
                if !base.is_empty() {
                    fp.use_targets.push(base.clone());
                    fp.wildcard_imports.push(base.clone());
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
            } else if fp.lang == Lang::Java {
                if let Some(pkg) = java_prelude_package(head) {
                    // Bare JDK type (java.lang/util) with no import — keep it as a
                    // stable external path instead of fabricating
                    // `<caller_pkg>::Type::…`, whose leaf collides with internal
                    // symbols and pollutes `ff cortex doctor`.
                    join(pkg, raw)
                } else if head.chars().next().is_some_and(|c| c.is_uppercase()) {
                    join(&fp.module, raw)
                } else {
                    raw.to_string()
                }
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

    // bare identifier (no ::) -> an imported free function (alias map) FIRST,
    // else the caller's own module. `use crate::m::foo; ... foo()` is the
    // dominant Rust call form, exactly as imported fns dominate in TS/Java
    // (resolve_call_dotty already checks the alias map first here). Without this,
    // a bare call to an imported fn was misattributed to caller_module::foo —
    // fabricating a code:extern in the WRONG module and leaving the real
    // function with fan-in 0, so callers/impact/tests under-counted across every
    // cross-module Rust call. A name can't be both `use`-imported and a local
    // item in the same scope (Rust rejects it), so alias-first is unambiguous.
    if !raw.contains("::") {
        if let Some(full) = alias_map.get(raw) {
            return resolve_alias_target(full, &caller_module);
        }
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
                join(&resolve_alias_target(full, &caller_module), &rest)
            } else {
                // Could be a sibling submodule of the caller's module, or an
                // already-qualified external/std path. Heuristic: if the head
                // looks like a crate (matches our crate name) keep as-is; if it
                // matches a known top-level (has more than one segment AND looks
                // external) keep as-is; otherwise treat as caller-module-relative.
                if head == crate_name
                    || segs.len() >= 3
                    || looks_external(head)
                    || is_std_prelude_type(head)
                    || is_std_prelude_trait(head)
                {
                    norm_crate(raw, crate_name)
                } else {
                    join(&caller_module, raw)
                }
            }
        }
    }
}

/// Rust glob-import fallback for a bare call that `resolve_call` could not point
/// at a known internal fn. A `use <prefix>::*;` in scope (most commonly
/// `mod tests { use super::*; }`) brings sibling/parent items into scope, so a
/// bare `foo()` may name `<prefix>::foo`. For each glob prefix — resolved
/// relative to the call site for `super`/`self` — return the first
/// `<prefix>::<name>` that names a REAL internal fn. No fabrication: this only
/// redirects to a symbol that already exists in the corpus, otherwise the call
/// keeps its original (extern) resolution.
fn resolve_glob_call(
    raw: &str,
    caller_qn: &str,
    fp: &FileParse,
    internal: &HashSet<String>,
) -> Option<String> {
    // Only Rust bare idents that weren't already bound by an explicit `use`.
    if fp.lang != Lang::Rust
        || raw.contains("::")
        || fp.glob_imports.is_empty()
        || fp.alias_map.contains_key(raw)
    {
        return None;
    }
    let caller_module = caller_qn
        .rsplit_once("::")
        .map(|(m, _)| m.to_string())
        .unwrap_or_else(|| fp.module.clone());
    for prefix in &fp.glob_imports {
        if let Some(base) = resolve_glob_prefix(prefix, &caller_module) {
            let cand = join(&base, raw);
            if internal.contains(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

/// Wildcard-import fallback for the dotty languages (Java/TS/Python), the
/// non-Rust analogue of [`resolve_glob_call`]. A Java `import static a.b.C.*;`
/// (static members) or `import a.b.*;` (package), or a Python `from m import *`,
/// brings members into scope WITHOUT binding an alias leaf. So a bare call
/// `check()` to such a member falls through `resolve_call_dotty`'s alias-map miss
/// to `<caller_module>::check` — a `code:extern` fabricated under the CALLER's own
/// path, leaving the real `a::b::C::check` with no fan-in. (Rust globs are handled
/// separately by [`resolve_glob_call`]; this covers the languages that have no
/// `super::*`-relative form, so the wildcard base is already absolute.)
///
/// Only fires for a bare leaf the alias map didn't already bind, and only when
/// EXACTLY ONE wildcard base yields a `<base>::<leaf>` that names a known internal
/// fn — if two wildcard imports both expose the leaf the binding is genuinely
/// ambiguous, so we leave the original (extern) resolution rather than guess.
/// Redirect-only: the target must already exist in the corpus, so it never
/// fabricates an edge.
fn resolve_wildcard_call(raw: &str, fp: &FileParse, internal: &HashSet<String>) -> Option<String> {
    // Dotty languages only; bare leaf only; must not already be alias-bound (a
    // named import is the more specific match and already resolved).
    if fp.lang == Lang::Rust
        || raw.contains("::")
        || fp.wildcard_imports.is_empty()
        || fp.alias_map.contains_key(raw)
    {
        return None;
    }
    let mut hit: Option<String> = None;
    for base in &fp.wildcard_imports {
        let cand = join(base, raw);
        if internal.contains(&cand) {
            if let Some(prev) = &hit {
                // A second distinct internal candidate → ambiguous, bail out.
                if prev != &cand {
                    return None;
                }
            } else {
                hit = Some(cand);
            }
        }
    }
    hit
}

/// Inherent-impl method redirect. A Rust method `fn bar()` inside `impl Foo` is
/// indexed flattened as `module::bar` (NOT `module::Foo::bar`) — a deliberate
/// tradeoff so a bare in-body call resolves to a sibling free fn. So an
/// associated-call site `Foo::bar()` resolves to the extern `module::Foo::bar`
/// while the real method sits at `module::bar`, splitting the method's fan-in
/// onto a per-type phantom. `ff cortex doctor` flagged this as the top internal
/// mis-resolution source (`AgentToolResult::ok`/`err`, the `X::new` collisions).
///
/// When the resolved name has the shape `<P>::<Type>::<leaf>` where `<P>::<Type>`
/// is a known internal type AND `<P>::<leaf>` is a known internal fn, redirect to
/// `<P>::<leaf>`. Redirect-only — it never fabricates: both the type and the
/// target method must already exist, so it's a no-op unless the method really
/// lives in that type's own module. The narrow risk (a free fn coincidentally
/// sharing the leaf name in the same module) is the same flattening collision the
/// indexer already accepts, never a fabricated edge.
fn resolve_impl_method_call(
    resolved: &str,
    internal_fns: &HashSet<String>,
    internal_types: &HashSet<String>,
) -> Option<String> {
    // Split `<P>::<Type>::<leaf>` → parent_type=`<P>::<Type>`, leaf=`<leaf>`.
    let (parent_type, leaf) = resolved.rsplit_once("::")?;
    // The type must itself carry a module prefix (`<P>::<Type>`), so a bare
    // two-segment `Type::leaf` (no owning module) can't accidentally redirect.
    let (module, _type_seg) = parent_type.rsplit_once("::")?;
    if !internal_types.contains(parent_type) {
        return None;
    }
    let target = join(module, leaf);
    internal_fns.contains(&target).then_some(target)
}

/// Glob-imported inherent-impl method redirect (lever #2b). When a `Type::method()`
/// associated call appears inside a `mod tests { use super::*; }` block,
/// `resolve_call` fabricates `<tests_module>::Type::method` (uppercase head →
/// same-module type), so the owning type segment carries the *test* submodule
/// instead of the type's real module. [`resolve_impl_method_call`] then can't fire
/// because `<tests_module>::Type` isn't a known internal type. Re-anchor the type
/// segment through the file's glob imports (the same mechanism as
/// [`resolve_glob_call`]): for each `use <prefix>::*`, resolve `<prefix>` relative
/// to the caller's module; if `<base>::Type` is a known internal type AND
/// `<base>::method` a known internal fn, redirect there. Redirect-only — both must
/// already exist, so it never fabricates. Conservatively gated to the
/// `<caller_module>::Type::leaf` shape the resolver itself fabricated; a genuine
/// cross-module path is left to the direct redirect.
fn resolve_glob_impl_method_call(
    resolved: &str,
    fp: &FileParse,
    caller_qn: &str,
    internal_fns: &HashSet<String>,
    internal_types: &HashSet<String>,
) -> Option<String> {
    if fp.lang != Lang::Rust || fp.glob_imports.is_empty() {
        return None;
    }
    // Split `<P>::<Type>::<leaf>`.
    let (parent_type, leaf) = resolved.rsplit_once("::")?;
    let (module, type_seg) = parent_type.rsplit_once("::")?;
    // Only re-anchor a type the resolver fabricated into the caller's own module
    // (`<caller_module>::<Type>::<leaf>`).
    let caller_module = caller_qn
        .rsplit_once("::")
        .map(|(m, _)| m)
        .unwrap_or(fp.module.as_str());
    if module != caller_module {
        return None;
    }
    for prefix in &fp.glob_imports {
        if let Some(base) = resolve_glob_prefix(prefix, caller_module) {
            let cand_type = join(&base, type_seg);
            if internal_types.contains(&cand_type) {
                let target = join(&base, leaf);
                if internal_fns.contains(&target) {
                    return Some(target);
                }
            }
        }
    }
    None
}

/// `Self::method()` redirect (lever #5). Inside an `impl` block, `Self::method()`
/// is an associated call on the type being implemented. `resolve_call` doesn't
/// special-case the `Self` keyword, so it treats the head as a same-module path
/// segment and fabricates `<caller_module>::Self::method`. But methods are indexed
/// flattened as `module::method` (the same convention [`resolve_impl_method_call`]
/// relies on), so the real target is `<caller_module>::method`. Redirect when that
/// names a known internal fn.
///
/// `Self` is a reserved Rust keyword and can never be a real module/item segment,
/// so a `::Self::` infix only ever appears via this fabrication — there's no
/// genuine path to confuse it with. Redirect-only: an external `Self::` trait
/// method (e.g. `Self::deserialize` from a derive) whose flattened target isn't in
/// the corpus keeps its extern resolution, so no edge is invented.
fn resolve_self_method_call(resolved: &str, internal_fns: &HashSet<String>) -> Option<String> {
    let (module, rest) = resolved.split_once("::Self::")?;
    let target = join(module, rest);
    internal_fns.contains(&target).then_some(target)
}

/// Java constructor redirect. `collect_java_calls` records `new Foo(...)` as a
/// call to `Foo::Foo` (the constructor leaf duplicates the type), which
/// `resolve_call_dotty` then prefixes to `<module>::Foo::Foo`. When the class has
/// an explicit constructor that whole path is a real `code:function` and resolves
/// directly; but the common case is a class with only the implicit/default
/// constructor — there is no `<module>::Foo::Foo` symbol, so the call fragments
/// onto a per-class extern (`AiClient::AiClient`, `SecurityConfig::SecurityConfig`
/// — flagged by `ff cortex doctor`) and the class node gets no instantiation
/// edge. All the other recall levers are Rust-only, so Java internal-resolution
/// sits far lower (≈18%) with zero heuristic redirects.
///
/// When the resolved path has the constructor shape `<P>::<Type>::<Type>` (the
/// trailing two segments identical) and `<P>::<Type>` is a known internal type,
/// redirect the edge onto the class node `<P>::<Type>`. Semantically the edge
/// then reads "this code instantiates `<Type>`", so `cortex callers <Type>`
/// surfaces instantiation sites. Redirect-only: the target type must already
/// exist in the corpus, so it never fabricates an edge; and it fires only when no
/// explicit-constructor fn matched (gated by `!internal_fns.contains` at the call
/// site), so a real constructor symbol always wins. A method named identically to
/// its own class is illegal Java, so the `::Type::Type` shape is unambiguously a
/// constructor.
///
/// J2 — nested classes. `new Inner()` where `Inner` is nested in `Outer` is
/// recorded as the caller-package path `<caller_pkg>::Inner::Inner`, but the real
/// class node lives at `<owner_pkg>::Outer::Inner` — the type segment dropped its
/// OUTER class (and the package may differ from the caller's), so the direct
/// `internal_types.contains(parent_type)` test misses it. Fall back to suffix-
/// matching `<type_seg>` across internal types: a candidate `…::<Outer>::<Inner>`
/// qualifies only when its own prefix (`…::<Outer>`) is itself a known internal
/// type — i.e. the leaf really is a nested class, not a top-level type that merely
/// shares a name. Gated to a SINGLE candidate, so an ambiguous nested name stays
/// extern. Still redirect-only (the target is a known internal type).
fn resolve_java_ctor_call(
    resolved: &str,
    internal_types: &HashSet<String>,
    type_by_leaf: &HashMap<String, Vec<String>>,
) -> Option<String> {
    let (parent_type, leaf) = resolved.rsplit_once("::")?;
    let type_seg = parent_type
        .rsplit_once("::")
        .map(|(_, t)| t)
        .unwrap_or(parent_type);
    if leaf != type_seg {
        return None;
    }
    // Direct case: the parent path is itself a known internal type (top-level, or
    // an already-correctly-qualified nested class).
    if internal_types.contains(parent_type) {
        return Some(parent_type.to_string());
    }
    // J2 nested-class case: find the real `…::<Outer>::<type_seg>` by suffix-
    // matching the leaf, keeping only candidates whose outer prefix is also an
    // internal type. Exactly one survivor → redirect; zero/ambiguous → extern.
    let mut nested = type_by_leaf.get(type_seg)?.iter().filter(|cand| {
        cand.rsplit_once("::")
            .is_some_and(|(outer, _)| internal_types.contains(outer))
    });
    match (nested.next(), nested.next()) {
        (Some(only), None) => Some(only.clone()),
        _ => None,
    }
}

/// J2b: nested-class static-method call recall. `Message.system(...)` where
/// `Message` is a class nested in `AiClient` is recorded by `resolve_call_dotty`
/// as the caller's own package path `<caller_pkg>::Message::system`, but the real
/// method node lives at `<owner_pkg>::AiClient::Message::system` (Java methods
/// nest under their class, `module::Outer::Inner::method`). The receiver TYPE
/// segment dropped its OUTER class — exactly the J2 constructor pattern, except
/// the trailing segment is a METHOD (`leaf != type_seg`) rather than a ctor.
///
/// Suffix-match the receiver type leaf across internal types via `type_by_leaf`,
/// keeping only candidates whose own outer prefix is itself an internal type
/// (i.e. genuine nested classes — a top-level type's prefix is a package, so it's
/// excluded, which is correct: a missed cross-package call is the alias map's job,
/// not this redirect's). Take the single survivor, reattach the method leaf, and
/// fire only when `<real_type>::<leaf>` is a known internal fn — triple-gated
/// (single nested candidate + internal-outer filter + final internal-fn check),
/// so it never fabricates an edge. Lowercase receivers (instance-variable calls
/// like `msg.system()`) are left alone — only `Type.method(...)` receivers redirect.
fn resolve_java_method_call(
    resolved: &str,
    internal_fns: &HashSet<String>,
    internal_types: &HashSet<String>,
    type_by_leaf: &HashMap<String, Vec<String>>,
) -> Option<String> {
    let (receiver, leaf) = resolved.rsplit_once("::")?;
    let type_seg = receiver
        .rsplit_once("::")
        .map(|(_, t)| t)
        .unwrap_or(receiver);
    if leaf == type_seg {
        return None; // constructor shape — handled by resolve_java_ctor_call.
    }
    if !type_seg.chars().next().is_some_and(|c| c.is_uppercase()) {
        return None; // lowercase receiver = an instance variable, not a type.
    }
    let mut nested = type_by_leaf.get(type_seg)?.iter().filter(|cand| {
        cand.rsplit_once("::")
            .is_some_and(|(outer, _)| internal_types.contains(outer))
    });
    let real_type = match (nested.next(), nested.next()) {
        (Some(only), None) => only,
        _ => return None, // zero or ambiguous → leave as extern.
    };
    let candidate = format!("{real_type}::{leaf}");
    internal_fns.contains(&candidate).then_some(candidate)
}

/// J2c: enum-constant / nested-member method redirect. Two Java idioms record a
/// call with one EXTRA qualifier segment between the owning type and the method,
/// so the primary resolver fabricates a per-member extern instead of hitting the
/// real method node:
///
///   - Enum-constant instance methods: `ErrorCode.INTERNAL_ERROR.status()` is
///     recorded as `<pkg>::ErrorCode::INTERNAL_ERROR::status`, but the method is
///     indexed under its declaring type as `<pkg>::ErrorCode::status` (the enum
///     CONSTANT `INTERNAL_ERROR` is a value, not a scope).
///   - Outer-class helpers from a `@Nested` inner class: a JUnit
///     `ConnectorFrameworkUnitTests.DisabledByDefaultContract` test calling the
///     OUTER class helper `apiEx()` records
///     `<pkg>::ConnectorFrameworkUnitTests::DisabledByDefaultContract::apiEx`,
///     but the helper lives on the outer class: `…::ConnectorFrameworkUnitTests::apiEx`.
///
/// Both reduce to the same shape — `<owner>::<Mid>::<leaf>` where dropping the
/// single middle segment `<Mid>` lands on the real method `<owner>::<leaf>`. This
/// is the inverse of J2b ([`resolve_java_method_call`], which ADDS the dropped
/// OUTER class), so it runs after it (J2b fails on these because the real method
/// is on the outer/declaring type, not the inner/constant segment).
///
/// Triple-gated, redirect-only: `<owner>` must be module-qualified AND a known
/// internal TYPE (the enum / outer class), and `<owner>::<leaf>` a known internal
/// fn — so it never fabricates. `<Mid>` must start uppercase: Java enum constants
/// are UPPER_SNAKE and nested classes UpperCamel (both capitalized), while an
/// instance method is lowerCamel — this excludes a fluent method chain
/// (`builder.build().toString()` recorded `Builder::build::toString`) from
/// redirecting `build`'s result onto `Builder::toString`.
fn resolve_java_member_method_call(
    resolved: &str,
    internal_fns: &HashSet<String>,
    internal_types: &HashSet<String>,
) -> Option<String> {
    // Split `<owner>::<Mid>::<leaf>` → head=`<owner>::<Mid>`, leaf.
    let (head, leaf) = resolved.rsplit_once("::")?;
    // Drop the middle segment: head=`<owner>::<Mid>` → owner, mid.
    let (owner, mid) = head.rsplit_once("::")?;
    // The owner must itself be module-qualified (`<pkg>::<Type>`), so a bare
    // two-segment owner can't accidentally redirect.
    if !owner.contains("::") {
        return None;
    }
    // The dropped segment is an enum constant or nested class — both capitalized
    // in Java. A lowercase segment is an instance-method receiver in a call chain,
    // not a value/type qualifier; leave those alone.
    if !mid.chars().next().is_some_and(|c| c.is_uppercase()) {
        return None;
    }
    if !internal_types.contains(owner) {
        return None;
    }
    let target = format!("{owner}::{leaf}");
    internal_fns.contains(&target).then_some(target)
}

/// Crate-root single-candidate redirect (lever #7). A bare call to a fn in a
/// sibling submodule that the alias map missed (`use crate::communities::*;`, an
/// inline `mod`, a re-export not modeled as `pub use`) falls through
/// `resolve_call_inner`'s bare-call path to `<caller_module>::<leaf>`; when the
/// caller is a free fn in the crate root, that is just `<crate>::<leaf>` (no
/// module segment). The real fn lives at `<crate>::<mod>::<leaf>`. If EXACTLY ONE
/// internal fn in that crate has the leaf, redirect to it.
///
/// Restricted to the 2-segment `<crate>::<leaf>` shape: a path that already
/// carries a module segment (`<crate>::<mod>::<leaf>`) is left alone, because a
/// genuine external-crate path (`libc::kill`, `serde_yaml::from_str`) has that
/// same 3+-segment shape and its leaf often collides with an unrelated internal
/// fn — only the no-module crate-rooted shape is reliably a missed sibling call.
/// Same-crate + single-candidate keeps it conservative; an ambiguous leaf (more
/// than one internal fn) stays extern. Redirect-only: fires only when the single
/// candidate is a known internal fn, so it never fabricates an edge.
fn resolve_crate_root_call(
    resolved: &str,
    crate_roots: &HashSet<String>,
    crate_leaf_fns: &HashMap<(String, String), Vec<String>>,
) -> Option<String> {
    let (krate, leaf) = resolved.split_once("::")?;
    if leaf.contains("::") {
        return None; // already module-qualified — not the missed-sibling shape.
    }
    if !crate_roots.contains(krate) {
        return None; // head isn't one of our crates — a genuine extern.
    }
    match crate_leaf_fns
        .get(&(krate.to_string(), leaf.to_string()))?
        .as_slice()
    {
        [only] => Some(only.clone()),
        _ => None, // zero or ambiguous → leave as extern.
    }
}

/// External-crate / std-primitive head redirect (lever #4 denoise). A bare
/// 3rd-party crate or std primitive used in associated-call position
/// (`hex::decode`, `jsonwebtoken::decode`, `f64::deserialize`) has a lowercase
/// head that isn't in the alias map, the caller's crate name, or the hand-kept
/// `looks_external` / std-prelude denylists, so `resolve_call_inner`'s fallback
/// fabricates `<caller_module>::<head>::<leaf>` — an internally-rooted extern
/// whose leaf then collides with real internal fns (`decode`, `deserialize`,
/// `from_path`) and pollutes the `ff cortex doctor` suspicious-extern list. When
/// the fabricated head is NOT a real child module of the caller (no internal
/// symbol lives under `<caller_module>::<head>`), strip the caller prefix back to
/// the already-qualified `<head>::<leaf>`: a single shared extern instead of a
/// per-caller-module phantom (the same idea as Java's `java_prelude_package`,
/// generalized off the corpus's own module set instead of a hand-maintained
/// crate list — so it doesn't need every 3rd-party crate enumerated).
///
/// Recall-neutral and never fabricates: it fires only on a CURRENT extern (the
/// caller guards `!internal_fns.contains`), and the child-module guard means a
/// genuine relative call (`<caller_module>::submod::foo`, where `submod` owns
/// internal symbols) is left untouched — only heads with no internal module
/// under them are collapsed. Uppercase heads (`Type::method`, `Trait::assoc`)
/// are owned by the std-prelude-type / impl-method / Self levers and skipped.
fn resolve_external_head_call(
    resolved: &str,
    caller_qn: &str,
    internal_modules: &HashSet<String>,
) -> Option<String> {
    // The caller's own module = caller_qn minus its leaf.
    let caller_module = caller_qn.rsplit_once("::").map(|(m, _)| m)?;
    let tail = resolved.strip_prefix(&format!("{caller_module}::"))?;
    // Must be the `<head>::<rest>` fabrication shape (an inner `::`), not a bare
    // leaf the caller-module default produced for an imported free fn.
    let (head, _rest) = tail.split_once("::")?;
    // Uppercase heads are types/traits — handled by the dedicated levers.
    if head.chars().next().is_some_and(|c| c.is_uppercase()) {
        return None;
    }
    // A real child module of the caller owns internal symbols under it — the
    // fabricated relative path was CORRECT, so leave it alone.
    if internal_modules.contains(&format!("{caller_module}::{head}")) {
        return None;
    }
    Some(tail.to_string())
}

/// Facade redirect (levers #3 + #4). Two failure modes leave a real internal
/// call resolved to a `code:extern`:
///
/// - #3 `pub use` re-exports — a facade crate (`ff_db`) re-exports a submodule
///   item, so `ff_db::run_postgres_migrations` is an alias for the real
///   `ff_db::migrations::run_postgres_migrations`; the call lands on the facade path.
/// - #4 crate-root caller-prefixing — `resolve_call` can't tell a crate-rooted
///   path (`ff_db::pg_get_brain_user`) from a same-module submodule, so it
///   fabricates `<caller_module>::ff_db::pg_get_brain_user`.
///
/// Combine both: re-anchor at a known crate root (#4), then chase the re-export
/// map to the real symbol (#3). Also handles a type-facade method receiver
/// (`ff_db::OperationalStore::postgres` → real type `ff_db::operational_store::OperationalStore` → flattened
/// method `ff_db::operational_store::postgres`). Redirect-only — every branch returns a name
/// that is already a known internal fn, so it never fabricates an edge.
fn resolve_facade_call(
    resolved: &str,
    reexports: &HashMap<String, String>,
    glob_reexports: &HashMap<String, Vec<String>>,
    crate_roots: &HashSet<String>,
    internal_fns: &HashSet<String>,
    internal_types: &HashSet<String>,
) -> Option<String> {
    // Candidate paths: the resolved name as-is, plus a crate-root-anchored suffix
    // if the resolver caller-prefixed a crate-qualified path.
    let mut cands = vec![resolved.to_string()];
    if let Some(anchored) = anchor_at_crate_root(resolved, crate_roots) {
        cands.push(anchored);
    }
    for cand in &cands {
        // (a) the anchored path is itself the real fn (#4 with no facade hop).
        if cand != resolved && internal_fns.contains(cand) {
            return Some(cand.clone());
        }
        // (b) function facade: chase `facade -> … -> real fn`.
        if let Some(t) = chase_reexport(cand, reexports, internal_fns) {
            return Some(t);
        }
        // (b') glob facade (#6): `<base>::<leaf>` re-exported by `pub use m::*;`.
        // The named-reexport map can't carry a glob (the leaf set is unknown at
        // parse time), so probe each glob target module of `<base>` for the leaf.
        if let Some((base, leaf)) = cand.rsplit_once("::") {
            if let Some(targets) = glob_reexports.get(base) {
                for target in targets {
                    let real = join(target, leaf);
                    if internal_fns.contains(&real) {
                        return Some(real);
                    }
                }
            }
        }
        // (c) type-facade method receiver: rewrite the type segment through the
        //     re-export map, then flatten via the inherent-impl convention.
        if let Some((parent_type, leaf)) = cand.rsplit_once("::") {
            if let Some(real_type) = chase_reexport(parent_type, reexports, internal_types) {
                if let Some((module, _)) = real_type.rsplit_once("::") {
                    let target = join(module, leaf);
                    if internal_fns.contains(&target) {
                        return Some(target);
                    }
                }
            }
        }
    }
    None
}

/// Follow a `pub use` re-export chain from `start` until it lands on a name in
/// `target_set` (the internal fn or type set). Bounded to a few hops to tolerate
/// re-export chains without looping. Returns `None` if no hop lands internal.
fn chase_reexport(
    start: &str,
    reexports: &HashMap<String, String>,
    target_set: &HashSet<String>,
) -> Option<String> {
    let mut cur = start;
    for _ in 0..4 {
        let next = reexports.get(cur)?;
        if target_set.contains(next) {
            return Some(next.clone());
        }
        cur = next;
    }
    None
}

/// If `resolved` carries a known crate root in a non-head position (the shape
/// `resolve_call` fabricates when it caller-prefixes a crate-qualified path,
/// `ff_gateway::brain_api::ff_db::foo`), return the suffix from that crate root
/// (`ff_db::foo`). Uses the LAST such root so the most specific (closest to the
/// leaf) anchor wins.
fn anchor_at_crate_root(resolved: &str, crate_roots: &HashSet<String>) -> Option<String> {
    let segs: Vec<&str> = resolved.split("::").collect();
    for i in (1..segs.len()).rev() {
        if crate_roots.contains(segs[i]) {
            return Some(segs[i..].join("::"));
        }
    }
    None
}

/// Resolve a `use`-alias target that may carry an unresolved relative prefix
/// (`use super::truncate_output;` → alias `truncate_output` → stored target
/// `super::truncate_output`). Only `crate::` is rewritten to an absolute path at
/// collection time (`norm_crate`); `super::`/`self::` are kept verbatim because
/// their meaning depends on the scope of the `use`, which we don't track at parse
/// time. We resolve them HERE, relative to the caller's module — correct for both
/// a top-of-file `use super::X` (the caller fn lives in the same module as the
/// `use`) and a `mod tests { use super::X; }` (the test fn's module's `super` is
/// the file module), because in Rust the call and its governing `use` always share
/// a module scope. Without this, every symbol imported via `use super::`/`use
/// self::` resolved to a literal `super::X` `code:extern` instead of its real
/// module path — 23 such phantoms on forge-fleet (`super::truncate_output`
/// fanin=30, `super::AgentToolResult::ok/err` fanin=57/52). Falls back to the
/// target unchanged if it isn't relative or if `super` walks above the crate root.
fn resolve_alias_target(target: &str, caller_module: &str) -> String {
    resolve_glob_prefix(target, caller_module).unwrap_or_else(|| target.to_string())
}

/// Resolve a glob-import module prefix to an absolute module path. `super`/`self`
/// are relative to `caller_module` (each `super` pops one trailing segment);
/// anything else is already fully qualified (`norm_crate` rewrote `crate::` at
/// collection time). Returns `None` if `super` walks above the crate root.
fn resolve_glob_prefix(prefix: &str, caller_module: &str) -> Option<String> {
    let mut segs = prefix.split("::");
    let first = segs.clone().next()?;
    if first != "super" && first != "self" {
        return Some(prefix.to_string()); // already absolute
    }
    let mut base: Vec<&str> = caller_module.split("::").collect();
    let mut rest: Vec<&str> = Vec::new();
    for seg in segs.by_ref() {
        match seg {
            "self" => {}
            "super" => {
                base.pop()?;
            }
            other => rest.push(other),
        }
    }
    base.extend(rest);
    if base.is_empty() {
        return None;
    }
    Some(base.join("::"))
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
            // 3rd-party crates the `ff cortex doctor` diagnostic surfaced as
            // caller-prefixed phantoms (`<mod>::toml::from_str`,
            // `<mod>::dirs::home_dir`): treat the bare crate head as already
            // qualified so it becomes a shared extern, not a per-module phantom.
            | "toml"
            | "dirs"
            | "base64"
            | "bincode"
            | "rand"
            | "tempfile"
            | "walkdir"
            | "url"
            // More 3rd-party crates `ff cortex doctor` surfaced as caller-prefixed
            // phantoms whose leaf then collides with an internal fn: `libc::kill`
            // (vs internal `kill`), `serde_yaml::from_str` (vs `from_str`),
            // `tracing_subscriber::registry` (vs `registry`). Keep the crate head
            // already-qualified so the call becomes a single shared extern.
            | "libc"
            | "serde_yaml"
            | "tracing_subscriber"
    )
}

/// Bare std-prelude TYPES used as an associated-call receiver: `Vec::new`,
/// `Arc::new`, `HashMap::with_capacity`, etc. The `other` head here is uppercase
/// and not in the alias map, so resolve_call_inner's fallback would treat it as a
/// same-module type and fabricate `<caller_module>::Vec::new` — a wrong,
/// internally-rooted extern that pollutes the caller's namespace and inflates its
/// extern fan-in (the `ff cortex doctor` diagnostic surfaced this as a top noise
/// source). These names are part of the Rust prelude / std and are essentially
/// never shadowed by a local type, so keep the call as-written (it becomes a
/// shared `Vec::new` extern, not a per-module phantom). If a project DID define
/// its own `Vec`, the call merely stays an extern instead of mis-attributing to
/// an internal symbol — same conservative failure mode, never worse.
fn is_std_prelude_type(head: &str) -> bool {
    matches!(
        head,
        // collections
        "Vec" | "HashMap"
            | "HashSet"
            | "BTreeMap"
            | "BTreeSet"
            | "VecDeque"
            | "BinaryHeap"
            | "LinkedList"
            // smart pointers / cells
            | "Box"
            | "Rc"
            | "Arc"
            | "Cow"
            | "Cell"
            | "RefCell"
            | "Mutex"
            | "RwLock"
            // strings / paths
            | "String"
            | "PathBuf"
            | "Path"
            | "OsString"
            | "CString"
            // time / misc std
            | "Duration"
            | "Instant"
            | "SystemTime"
    )
}

/// Bare std-prelude TRAITS used as an associated-call receiver: `Default::default`,
/// `From::from`, `Into::into`, `FromStr::from_str`, etc. Same failure mode as
/// [`is_std_prelude_type`] — the uppercase head isn't aliased, so the fallback
/// fabricates a `<caller_module>::Default::default` phantom whose `default`/`from`
/// leaf then collides with every internal `default`/`from` fn (`ff cortex doctor`
/// surfaced `…::tests::Default::default` etc. as residual internally-rooted noise).
/// These are std prelude traits, effectively never shadowed by a local type, so
/// keep the call as a shared extern instead of a per-module phantom. Conservative:
/// a project that genuinely defined its own `From` merely leaves the call an extern
/// rather than mis-attributing it — never worse.
fn is_std_prelude_trait(head: &str) -> bool {
    matches!(
        head,
        "Default"
            | "From"
            | "Into"
            | "TryFrom"
            | "TryInto"
            | "FromStr"
            | "FromIterator"
            | "ToString"
            | "ToOwned"
            | "Clone"
            | "AsRef"
            | "AsMut"
            | "Iterator"
            | "IntoIterator"
    )
}

/// Bare JDK prelude TYPES used as a static-call / constant receiver with no
/// import (`Boolean.TRUE.equals(x)`, `Integer.parseInt(s)`, `Math.max(a,b)`,
/// `Objects.requireNonNull(x)`). `java.lang` is auto-imported and `java.util`
/// statics are pervasive, so these heads appear bare and uppercase — the Java
/// analog of [`is_std_prelude_type`]. Without this, `resolve_call_dotty`'s
/// uppercase fallback fabricates `<caller_pkg>::Boolean::TRUE::equals`, an
/// internally-rooted extern whose leaf (`equals`/`parseInt`/…) then collides with
/// real internal methods — the #1 `ff cortex doctor` noise source on the Java
/// corpora. Returns the canonical JDK package so the call becomes a single shared
/// `java::lang::Boolean::…` extern (head not a corpus module root, so the
/// diagnostic stops flagging it). Restricted to final/effectively-final JDK
/// classes that are essentially never shadowed by a user type; a project that DID
/// define its own merely leaves the call an extern, never mis-attributed — same
/// conservative failure mode as the Rust prelude levers. Collection *interfaces*
/// (List/Map/Set/Optional) are deliberately omitted: they're more plausibly
/// user-shadowed and rarely the receiver of a bare static call.
fn java_prelude_package(head: &str) -> Option<&'static str> {
    match head {
        // java.lang (auto-imported, final boxed primitives + core utilities)
        "Boolean" | "Byte" | "Short" | "Integer" | "Long" | "Float" | "Double" | "Character"
        | "String" | "Math" | "System" | "Class" | "Thread" | "Runtime" | "StringBuilder"
        | "StringBuffer" => Some("java::lang"),
        // java.util (ubiquitous static-method holders)
        "Objects" | "Arrays" | "Collections" => Some("java::util"),
        // java.util.stream
        "Collectors" => Some("java::util::stream"),
        _ => None,
    }
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

    fn sample_export() -> GraphExport {
        GraphExport {
            nodes: vec![
                ("id1".into(), "foo<T> & bar".into(), "function".into()),
                ("id2".into(), "baz".into(), "struct".into()),
            ],
            edges: vec![("id1".into(), "id2".into(), "calls".into())],
        }
    }

    #[test]
    fn graphml_is_well_formed_and_escapes() {
        let xml = graph_to_graphml(&sample_export());
        assert!(xml.starts_with("<?xml version=\"1.0\""));
        assert!(xml.contains("<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\">"));
        assert!(xml.trim_end().ends_with("</graphml>"));
        // Special chars in the label are XML-escaped, not raw.
        assert!(xml.contains("foo&lt;T&gt; &amp; bar"));
        assert!(!xml.contains("foo<T> & bar"));
        // One node element per node, one edge element per edge.
        assert_eq!(xml.matches("<node ").count(), 2);
        assert_eq!(xml.matches("<edge ").count(), 1);
        assert!(xml.contains("source=\"id1\" target=\"id2\""));
    }

    #[test]
    fn dot_quotes_and_escapes() {
        let dot = graph_to_dot(&sample_export());
        assert!(dot.starts_with("digraph cortex {"));
        assert!(dot.trim_end().ends_with("}"));
        assert!(dot.contains("\"id1\" -> \"id2\";"));
        // The label is shown (not the id) and quotes/backslashes are escaped.
        assert!(dot.contains("[label=\"foo<T> & bar\"]"));
    }

    #[test]
    fn empty_export_serializes_cleanly() {
        let g = GraphExport::default();
        assert!(graph_to_graphml(&g).contains("<graph edgedefault=\"directed\">"));
        assert!(graph_to_dot(&g).contains("digraph cortex {"));
    }

    #[test]
    fn no_symbol_error_names_selector_and_corpus() {
        // The symbol-keyed query verbs (callers/callees/impact/tests_for) share
        // this message when resolution finds nothing — keep it uniform so the
        // wording (and a downstream test/grep on it) stays stable.
        let e = no_symbol_error("load_model", "forge-fleet");
        assert_eq!(
            e.to_string(),
            "no symbol matching 'load_model' in corpus 'forge-fleet'"
        );
    }

    #[test]
    fn reconstruct_path_walks_parents_in_forward_order() {
        // Simulate a BFS parent map for a chain a → b → c → d (a = source, no
        // entry). reconstruct_path(.., d) must stitch the full forward chain.
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        let d = Uuid::from_u128(4);
        let mut parent = HashMap::new();
        parent.insert(b, a);
        parent.insert(c, b);
        parent.insert(d, c);
        // A second branch off `a` (a → e) must not contaminate the d-chain.
        let e = Uuid::from_u128(5);
        parent.insert(e, a);
        assert_eq!(reconstruct_path(&parent, d), vec![a, b, c, d]);
        // A node reached directly from a source is a single-hop chain.
        assert_eq!(reconstruct_path(&parent, e), vec![a, e]);
        // A source itself (no parent entry) reconstructs to just itself.
        assert_eq!(reconstruct_path(&parent, a), vec![a]);
    }

    #[test]
    fn call_confidence_tiers() {
        // EXTRACTED: primary resolver named a real internal symbol.
        assert_eq!(call_confidence(true, true), 1.0);
        // INFERRED: a heuristic redirect landed it on an internal symbol.
        assert_eq!(call_confidence(false, true), 0.6);
        // UNRESOLVED: callee is an extern placeholder — regardless of primary hit.
        assert_eq!(call_confidence(false, false), 0.3);
        assert_eq!(call_confidence(true, false), 0.3);
        // The three internal/extern tiers are strictly ordered (consumers filter
        // on `>= 1.0` for high-trust, `>= 0.6` to include heuristics).
        assert!(call_confidence(true, true) > call_confidence(false, true));
        assert!(call_confidence(false, true) > call_confidence(false, false));
    }

    #[test]
    fn escape_like_neutralizes_wildcards() {
        // A literal underscore in a Rust name must stay literal, not match-any.
        assert_eq!(escape_like("load_model"), r"load\_model");
        // % and the escape char itself are escaped too; plain names pass through.
        assert_eq!(escape_like("a%b_c\\d"), r"a\%b\_c\\d");
        assert_eq!(escape_like("plainName"), "plainName");
        assert_eq!(escape_like(""), "");
    }

    fn hit(name: &str, fan_in: i64) -> SymbolHit {
        SymbolHit {
            id: Uuid::nil(),
            qualified_name: name.to_string(),
            node_type: "code:function".to_string(),
            file: Some("/x.rs".to_string()),
            start_line: Some(1),
            fan_in,
            score: None,
        }
    }

    #[test]
    fn leaf_name_takes_last_segment() {
        assert_eq!(leaf_name("a::b::load_model"), "load_model");
        assert_eq!(leaf_name("pkg.module.LoginPage"), "LoginPage");
        assert_eq!(leaf_name("bare"), "bare");
        // `::` wins over `.` when both appear (Rust-style path with a dotted leaf
        // is unusual, but the `::` split is checked first by design).
        assert_eq!(leaf_name("a::b.c"), "b.c");
    }

    #[test]
    fn pick_show_match_prefers_exact_then_leaf_then_top() {
        // hits arrive fan-in-desc (highest first), as find_symbols returns them.
        let hits = vec![
            hit("ff_x::loader::load_model_async", 9),
            hit("ff_y::load_model", 5),
            hit("ff_z::other::load_model", 3),
        ];
        // 1. exact qualified-name match wins regardless of fan-in order.
        assert_eq!(pick_show_match(&hits, "ff_y::load_model"), 1);
        // 2. no exact qualified match → exact leaf match, highest fan-in (first such).
        assert_eq!(pick_show_match(&hits, "load_model"), 1);
        // 3. neither → top hit (index 0, highest fan-in).
        assert_eq!(pick_show_match(&hits, "load"), 0);
        // case-insensitive on both qualified and leaf.
        assert_eq!(pick_show_match(&hits, "LOAD_MODEL"), 1);
    }

    #[test]
    fn slice_source_lines_extracts_inclusive_range_and_caps() {
        let src = "l1\nl2\nl3\nl4\nl5";
        // Inclusive 1-based [2,4] → l2,l3,l4, not truncated.
        let (s, trunc) = slice_source_lines(src, 2, 4, 100);
        assert_eq!(s, "l2\nl3\nl4");
        assert!(!trunc);
        // max_lines caps the span and flags truncation.
        let (s, trunc) = slice_source_lines(src, 1, 5, 2);
        assert_eq!(s, "l1\nl2");
        assert!(trunc);
        // Single-line span.
        let (s, trunc) = slice_source_lines(src, 3, 3, 100);
        assert_eq!(s, "l3");
        assert!(!trunc);
        // Defensive: start clamps to 1, end clamps up to start, max_lines floors at 1.
        let (s, _) = slice_source_lines(src, 0, -5, 0);
        assert_eq!(s, "l1");
    }

    #[test]
    fn context_display_range_widens_and_clamps() {
        // context 0 → exactly the symbol span.
        assert_eq!(context_display_range(10, 20, 0), (10, 20));
        // context N widens both edges by N.
        assert_eq!(context_display_range(10, 20, 3), (7, 23));
        // leading edge clamps to line 1 (never 0 or negative).
        assert_eq!(context_display_range(2, 5, 10), (1, 15));
    }

    #[test]
    fn file_match_pattern_escapes_and_anchors_on_separator() {
        // A bare basename anchors on a path separator so `cortex.rs` can't match
        // `mycortex.rs`, and LIKE wildcards in the arg are escaped.
        assert_eq!(file_match_pattern("cortex.rs"), "%/cortex.rs");
        assert_eq!(file_match_pattern("a_b.rs"), "%/a\\_b.rs");
        assert_eq!(file_match_pattern("src/cortex.rs"), "%/src/cortex.rs");
    }

    #[test]
    fn choose_file_match_prefers_exact_then_unique_then_ambiguous() {
        // No candidates.
        assert_eq!(choose_file_match(&[], "x.rs"), FileChoice::None);
        // Exact full-path match wins even when other suffix matches exist.
        let paths = ["/a/mod.rs", "/b/c/mod.rs"];
        assert_eq!(
            choose_file_match(&paths, "/b/c/mod.rs"),
            FileChoice::Unique(1)
        );
        // A single candidate is taken (suffix match, no exact).
        assert_eq!(
            choose_file_match(&["/x/y/cortex.rs"], "cortex.rs"),
            FileChoice::Unique(0)
        );
        // Multiple suffix matches, no exact → ambiguous (caller disambiguates).
        assert_eq!(choose_file_match(&paths, "mod.rs"), FileChoice::Ambiguous);
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
    fn demote_delete_splits_on_surviving_caller() {
        let a = Uuid::from_u128(1); // removed, still called by a survivor → demote
        let b = Uuid::from_u128(2); // removed, no surviving caller → delete
        let c = Uuid::from_u128(3); // removed, called only by another removed node → delete
        let removed = [a, b, c];
        // `still_called` is the SQL result: removed nodes a survivor calls into.
        // `c` is excluded by the `src_id <> ALL(removed)` guard, so it is NOT here.
        let still_called = [a];
        let (demote, delete) = partition_demote_delete(&removed, &still_called);
        assert_eq!(demote, vec![a]);
        let del: HashSet<Uuid> = delete.into_iter().collect();
        assert_eq!(del, HashSet::from([b, c]));
    }

    #[test]
    fn demote_delete_ignores_unknown_and_empty() {
        // A `still_called` id not in `removed` is ignored (can't demote what we
        // weren't asked to remove); empty in → empty out.
        let a = Uuid::from_u128(1);
        let stray = Uuid::from_u128(9);
        let (demote, delete) = partition_demote_delete(&[a], &[stray]);
        assert!(demote.is_empty());
        assert_eq!(delete, vec![a]);
        let (d2, del2) = partition_demote_delete(&[], &[]);
        assert!(d2.is_empty());
        assert!(del2.is_empty());
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

    /// Build the leaf->types index the way `extract_files` does, for ctor tests.
    fn type_leaf_index(types: &HashSet<String>) -> HashMap<String, Vec<String>> {
        let mut by_leaf: HashMap<String, Vec<String>> = HashMap::new();
        for t in types {
            if let Some(leaf) = t.rsplit("::").next() {
                by_leaf.entry(leaf.to_string()).or_default().push(t.clone());
            }
        }
        by_leaf
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
            glob_imports: vec![],
            reexports: vec![],
            glob_reexports: vec![],
            wildcard_imports: vec![],
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
    fn bare_imported_fn_resolves_via_alias_not_caller_module() {
        // agent_loop.rs:709 `should_compact(` inside fn run_agent_loop, with
        // `use crate::compaction::should_compact;` (alias should_compact ->
        // ff_agent::compaction::should_compact). The bare call must resolve to
        // the IMPORTED function, not a fabricated ff_agent::agent_loop::should_compact.
        let fp = fp_with(
            "ff_agent::agent_loop",
            "ff_agent",
            &[("should_compact", "ff_agent::compaction::should_compact")],
        );
        let got = resolve_call(
            "should_compact",
            "ff_agent::agent_loop::run_agent_loop",
            &fp,
        );
        assert_eq!(got, "ff_agent::compaction::should_compact");
    }

    /// Build a dotty (Java/TS/Python) FileParse with a synthetic import map for the
    /// wildcard-resolution tests: `aliases` are named imports (bind a leaf),
    /// `wildcards` are the absolute bases of `import a.b.C.*;` / `from m import *`.
    fn fp_dotty(
        lang: Lang,
        module: &str,
        crate_name: &str,
        aliases: &[(&str, &str)],
        wildcards: &[&str],
    ) -> FileParse {
        let mut fp = fp_with(module, crate_name, aliases);
        fp.lang = lang;
        fp.wildcard_imports = wildcards.iter().map(|s| s.to_string()).collect();
        fp
    }

    #[test]
    fn java_static_wildcard_bare_call_resolves_to_imported_def() {
        // `import static com.acme.util.Asserts.*;` then a bare `check(user)` inside
        // com::acme::auth::AuthService::login. Without the wildcard lever the bare
        // call falls through the alias-map miss to the caller's own class
        // (com::acme::auth::AuthService::check) — a code:extern under the CALLER's
        // path. It must redirect to the real imported def.
        let fp = fp_dotty(
            Lang::Java,
            "com::acme::auth",
            "com::acme::auth",
            &[],
            &["com::acme::util::Asserts"],
        );
        let internal: HashSet<String> = ["com::acme::util::Asserts::check".to_string()].into();
        // Primary resolver fabricates the caller-prefixed extern...
        let primary = resolve_call("check", "com::acme::auth::AuthService::login", &fp);
        assert_eq!(primary, "com::acme::auth::AuthService::check");
        assert!(!internal.contains(&primary));
        // ...and the wildcard lever redirects it to the imported definition.
        assert_eq!(
            resolve_wildcard_call("check", &fp, &internal).as_deref(),
            Some("com::acme::util::Asserts::check")
        );
    }

    #[test]
    fn python_star_import_bare_call_resolves_to_imported_def() {
        // `from acme.helpers import *` then a bare `audit(user)`.
        let fp = fp_dotty(Lang::Python, "acme::api", "acme", &[], &["acme::helpers"]);
        let internal: HashSet<String> = ["acme::helpers::audit".to_string()].into();
        assert_eq!(
            resolve_wildcard_call("audit", &fp, &internal).as_deref(),
            Some("acme::helpers::audit")
        );
    }

    #[test]
    fn wildcard_call_no_redirect_when_target_not_internal() {
        // The wildcard base exposes the leaf, but no such internal fn exists — never
        // fabricate, leave the extern resolution untouched.
        let fp = fp_dotty(Lang::Java, "a::b", "a::b", &[], &["x::y::Z"]);
        let internal: HashSet<String> = ["x::y::Z::other".to_string()].into();
        assert_eq!(resolve_wildcard_call("check", &fp, &internal), None);
    }

    #[test]
    fn wildcard_call_ambiguous_two_bases_bails_out() {
        // Two static wildcards both expose `check` as a real internal fn — the
        // binding is genuinely ambiguous, so resolve to neither (no guessed edge).
        let fp = fp_dotty(Lang::Java, "a::b", "a::b", &[], &["x::A", "y::B"]);
        let internal: HashSet<String> =
            ["x::A::check".to_string(), "y::B::check".to_string()].into();
        assert_eq!(resolve_wildcard_call("check", &fp, &internal), None);
    }

    #[test]
    fn wildcard_call_named_import_wins_over_wildcard() {
        // A named static import (`import static x.A.check;`) binds the leaf in the
        // alias map; the wildcard lever must defer to it (it's the more specific
        // match) and not fire even though a wildcard base also exposes the leaf.
        let fp = fp_dotty(
            Lang::Java,
            "a::b",
            "a::b",
            &[("check", "x::A::check")],
            &["y::B"],
        );
        let internal: HashSet<String> =
            ["x::A::check".to_string(), "y::B::check".to_string()].into();
        // alias map already resolves it directly...
        assert_eq!(resolve_call("check", "a::b::Svc::m", &fp), "x::A::check");
        // ...so the wildcard lever is a no-op (alias-bound leaf is skipped).
        assert_eq!(resolve_wildcard_call("check", &fp, &internal), None);
    }

    #[test]
    fn wildcard_call_ignores_rust_and_dotted_and_empty() {
        let internal: HashSet<String> = ["a::b::check".to_string()].into();
        // Rust uses glob_imports, not this lever.
        let rust = fp_dotty(Lang::Rust, "a", "a", &[], &["a::b"]);
        assert_eq!(resolve_wildcard_call("check", &rust, &internal), None);
        // A dotted receiver call is not a bare leaf.
        let java = fp_dotty(Lang::Java, "a", "a", &[], &["a::b"]);
        assert_eq!(resolve_wildcard_call("C::check", &java, &internal), None);
        // No wildcard imports → nothing to do.
        let none = fp_dotty(Lang::Java, "a", "a", &[], &[]);
        assert_eq!(resolve_wildcard_call("check", &none, &internal), None);
    }

    #[test]
    fn use_super_alias_resolves_relative_to_caller_module() {
        // `crates/ff-agent/src/tools/bash.rs` (module ff_agent::tools::bash) does
        // `use super::{AgentToolResult, truncate_output};`. `super` is the parent
        // module (ff_agent::tools). The alias map stores the targets verbatim
        // (`super::truncate_output`) because only `crate::` is normalized at parse
        // time, so without relative resolution the bare call landed on a literal
        // `super::truncate_output` extern. It must resolve to ff_agent::tools::*.
        let fp = fp_with(
            "ff_agent::tools::bash",
            "ff_agent",
            &[
                ("truncate_output", "super::truncate_output"),
                ("AgentToolResult", "super::AgentToolResult"),
            ],
        );
        // Bare imported fn.
        assert_eq!(
            resolve_call("truncate_output", "ff_agent::tools::bash::execute", &fp),
            "ff_agent::tools::truncate_output"
        );
        // Scoped associated call through the imported type alias.
        assert_eq!(
            resolve_call(
                "AgentToolResult::err",
                "ff_agent::tools::bash::execute",
                &fp
            ),
            "ff_agent::tools::AgentToolResult::err"
        );
    }

    #[test]
    fn use_super_alias_in_test_mod_resolves_to_file_module() {
        // `mod think_strip_tests { use super::strip_think_blocks; }` in research.rs
        // (file module ff_agent::research). The test fn's module is
        // ff_agent::research::think_strip_tests, whose `super` is ff_agent::research
        // — resolving the alias relative to the CALLER module gets this right even
        // though the same alias text (`super::…`) means a different parent here.
        let fp = fp_with(
            "ff_agent::research::think_strip_tests",
            "ff_agent",
            &[("strip_think_blocks", "super::strip_think_blocks")],
        );
        assert_eq!(
            resolve_call(
                "strip_think_blocks",
                "ff_agent::research::think_strip_tests::strips_single_block",
                &fp
            ),
            "ff_agent::research::strip_think_blocks"
        );
    }

    #[test]
    fn std_prelude_types_are_not_caller_module_rooted() {
        // `Vec::new()` / `Arc::new()` etc. written bare: the head is uppercase and
        // unaliased, so the old fallback fabricated `<caller_module>::Vec::new`.
        // They must stay as written (a shared std extern), not get our module glued
        // on. Regression for the `ff cortex doctor` internally-rooted-extern noise.
        let fp = fp_with("ff_brain::cortex", "ff_brain", &[]);
        let caller = "ff_brain::cortex::run_index";
        assert_eq!(resolve_call("Vec::new", caller, &fp), "Vec::new");
        assert_eq!(resolve_call("Arc::new", caller, &fp), "Arc::new");
        assert_eq!(
            resolve_call("HashMap::with_capacity", caller, &fp),
            "HashMap::with_capacity"
        );
        assert_eq!(resolve_call("PathBuf::from", caller, &fp), "PathBuf::from");
        // A genuine same-module type (not a std prelude name) still resolves
        // caller-module-relative — the guard is narrow, not a blanket uppercase rule.
        assert_eq!(
            resolve_call("MyLocalType::new", caller, &fp),
            "ff_brain::cortex::MyLocalType::new"
        );
    }

    #[test]
    fn std_prelude_traits_are_not_caller_module_rooted() {
        // `Default::default()` / `From::from()` written bare in associated-call
        // position: same fabrication as the std types — the uppercase trait head
        // was getting the caller module glued on (`<mod>::Default::default`), whose
        // leaf collides with every internal `default`/`from`. Keep them as shared
        // std externs. Regression for the `ff cortex doctor` residual noise.
        let fp = fp_with("ff_core::chaos", "ff_core", &[]);
        let caller = "ff_core::chaos::tests::injects_fault";
        assert_eq!(
            resolve_call("Default::default", caller, &fp),
            "Default::default"
        );
        assert_eq!(resolve_call("From::from", caller, &fp), "From::from");
        assert_eq!(
            resolve_call("FromStr::from_str", caller, &fp),
            "FromStr::from_str"
        );
    }

    #[test]
    fn external_crate_heads_are_not_caller_module_rooted() {
        // `libc::kill`, `serde_yaml::from_str`, `tracing_subscriber::registry`:
        // 2-segment external-crate paths whose head isn't aliased. Without the
        // looks_external entry the resolver glued the caller module on
        // (`ff_agent::task_runner::libc::kill`), fragmenting the extern and
        // colliding its leaf with internal fns (the ff cortex doctor noise).
        let fp = fp_with("ff_agent::task_runner", "ff_agent", &[]);
        let caller = "ff_agent::task_runner::run_wave";
        assert_eq!(resolve_call("libc::kill", caller, &fp), "libc::kill");
        assert_eq!(
            resolve_call("serde_yaml::from_str", caller, &fp),
            "serde_yaml::from_str"
        );
        assert_eq!(
            resolve_call("tracing_subscriber::registry", caller, &fp),
            "tracing_subscriber::registry"
        );
    }

    #[test]
    fn crate_root_single_candidate_redirects_to_sibling_module() {
        // `community_member_hash()` called bare from a fn in ff_brain's crate root
        // (the `use` was missed) lands on `ff_brain::community_member_hash`; the
        // real fn is `ff_brain::communities::community_member_hash`. Exactly one
        // internal fn in the crate has that leaf, so redirect (ff cortex doctor
        // top 2-segment suspicious externs).
        let roots: HashSet<String> = ["ff_brain".to_string(), "ff_agent".to_string()].into();
        let mut leaf_fns: HashMap<(String, String), Vec<String>> = HashMap::new();
        leaf_fns.insert(
            ("ff_brain".to_string(), "community_member_hash".to_string()),
            vec!["ff_brain::communities::community_member_hash".to_string()],
        );
        assert_eq!(
            resolve_crate_root_call("ff_brain::community_member_hash", &roots, &leaf_fns),
            Some("ff_brain::communities::community_member_hash".to_string())
        );

        // Already module-qualified (external-crate shape) → left alone, so a real
        // `libc::kill` extern is never misredirected onto an internal `kill`.
        leaf_fns.insert(
            ("ff_agent".to_string(), "kill".to_string()),
            vec!["ff_agent::reaper::kill".to_string()],
        );
        assert_eq!(
            resolve_crate_root_call("ff_agent::task_runner::libc::kill", &roots, &leaf_fns),
            None
        );

        // Ambiguous (>1 internal fn in the crate with that leaf) → stays extern.
        let mut amb: HashMap<(String, String), Vec<String>> = HashMap::new();
        amb.insert(
            ("ff_agent".to_string(), "install".to_string()),
            vec![
                "ff_agent::template_registry::install".to_string(),
                "ff_agent::other::install".to_string(),
            ],
        );
        assert_eq!(
            resolve_crate_root_call("ff_agent::install", &roots, &amb),
            None
        );

        // Head isn't one of our crate roots → a genuine extern, untouched.
        assert_eq!(
            resolve_crate_root_call("libc::kill", &roots, &leaf_fns),
            None
        );
    }

    #[test]
    fn external_head_call_collapses_3rd_party_and_primitive_phantoms() {
        // A real child module of the caller owns symbols under it.
        let mods: HashSet<String> = [
            "ff_gateway".to_string(),
            "ff_gateway::middleware".to_string(),
            "ff_gateway::server".to_string(),
            "ff_orchestrator::scheduler".to_string(),
            // `ff_db::migrations` is a genuine child module (owns symbols).
            "ff_db".to_string(),
            "ff_db::migrations".to_string(),
        ]
        .into();

        // 3rd-party crate head `jsonwebtoken` fabricated onto the caller module →
        // collapse to the shared `jsonwebtoken::decode` extern.
        assert_eq!(
            resolve_external_head_call(
                "ff_gateway::middleware::jsonwebtoken::decode",
                "ff_gateway::middleware::auth",
                &mods,
            ),
            Some("jsonwebtoken::decode".to_string())
        );
        // Same crate-head from a DIFFERENT caller module collapses to the SAME
        // extern (fan-in consolidation, not a per-module phantom).
        assert_eq!(
            resolve_external_head_call(
                "ff_gateway::server::hex::decode",
                "ff_gateway::server::handler",
                &mods,
            ),
            Some("hex::decode".to_string())
        );
        // Std primitive head `f64` in associated-call position.
        assert_eq!(
            resolve_external_head_call(
                "ff_orchestrator::scheduler::f64::deserialize",
                "ff_orchestrator::scheduler::parse",
                &mods,
            ),
            Some("f64::deserialize".to_string())
        );
    }

    #[test]
    fn external_head_call_preserves_real_child_modules_and_types() {
        let mods: HashSet<String> = [
            "ff_db".to_string(),
            "ff_db::migrations".to_string(),
            "ff_gateway".to_string(),
            "ff_gateway::middleware".to_string(),
        ]
        .into();

        // `migrations` IS a real child module of ff_db → the relative path was
        // correct; do NOT strip it (would mis-name the extern / lose context).
        assert_eq!(
            resolve_external_head_call(
                "ff_db::migrations::run_postgres_migrations",
                "ff_db::boot",
                &mods,
            ),
            None
        );
        // Uppercase head (`Type::method`) is owned by the std-prelude-type /
        // impl-method levers — never touched here.
        assert_eq!(
            resolve_external_head_call(
                "ff_gateway::middleware::Foo::bar",
                "ff_gateway::middleware::auth",
                &mods,
            ),
            None
        );
        // Bare leaf (no inner `::`) is an imported free fn, not the head
        // fabrication shape — left alone.
        assert_eq!(
            resolve_external_head_call(
                "ff_gateway::middleware::helper",
                "ff_gateway::middleware::auth",
                &mods
            ),
            None
        );
        // Caller at crate root with no module segment → no caller prefix to strip.
        assert_eq!(
            resolve_external_head_call("hex::decode", "hex", &mods),
            None
        );
    }

    #[test]
    fn impl_method_call_redirects_to_flattened_method() {
        // `AgentToolResult::ok()` resolves to the extern `ff_agent::tools::
        // AgentToolResult::ok`, but the method is indexed flattened as
        // `ff_agent::tools::ok`. With both the type and the method known internal,
        // the redirect points the call at the real method node (the `ff cortex
        // doctor` top noise source).
        let types: HashSet<String> = ["ff_agent::tools::AgentToolResult".to_string()].into();
        let fns: HashSet<String> = ["ff_agent::tools::ok".to_string()].into();
        assert_eq!(
            resolve_impl_method_call("ff_agent::tools::AgentToolResult::ok", &fns, &types),
            Some("ff_agent::tools::ok".to_string())
        );
    }

    #[test]
    fn impl_method_redirect_never_fabricates() {
        // No redirect unless BOTH the owning type AND the target method exist.
        let types: HashSet<String> = ["ff_agent::tools::AgentToolResult".to_string()].into();
        let fns: HashSet<String> = HashSet::new();
        // Type known, method absent → no edge invented.
        assert_eq!(
            resolve_impl_method_call("ff_agent::tools::AgentToolResult::ok", &fns, &types),
            None
        );
        // Method present but the parent segment isn't a known type (a genuine
        // module path `a::b::c`) → left alone, not collapsed to `a::c`.
        let fns2: HashSet<String> = ["ff_agent::tools::ok".to_string()].into();
        let no_types: HashSet<String> = HashSet::new();
        assert_eq!(
            resolve_impl_method_call("ff_agent::tools::AgentToolResult::ok", &fns2, &no_types),
            None
        );
        // A bare two-segment `Type::leaf` (no owning module) can't redirect.
        let t2: HashSet<String> = ["AgentToolResult".to_string()].into();
        let f2: HashSet<String> = ["ok".to_string()].into();
        assert_eq!(
            resolve_impl_method_call("AgentToolResult::ok", &f2, &t2),
            None
        );
    }

    #[test]
    fn glob_impl_method_redirects_type_via_super_import() {
        // lever #2b: `mod tests { use super::*; }` then `NodeRegistry::new()` in a
        // test. resolve_call fabricates `<tests_module>::NodeRegistry::new`
        // (uppercase head → same-module type), so the direct redirect can't see the
        // real type. The glob-aware redirect re-anchors the type segment through
        // `use super::*` onto the parent module, then collapses to the flattened
        // method.
        let mut fp = fp_with("ff_agent::registry", "ff_agent", &[]);
        fp.glob_imports = vec!["super".to_string()];
        let caller = "ff_agent::registry::tests::builds_registry";
        let types: HashSet<String> = ["ff_agent::registry::NodeRegistry".to_string()].into();
        let fns: HashSet<String> = ["ff_agent::registry::new".to_string()].into();
        assert_eq!(
            resolve_glob_impl_method_call(
                "ff_agent::registry::tests::NodeRegistry::new",
                &fp,
                caller,
                &fns,
                &types,
            ),
            Some("ff_agent::registry::new".to_string())
        );
    }

    #[test]
    fn glob_impl_method_redirect_never_fabricates() {
        let mut fp = fp_with("ff_agent::registry", "ff_agent", &[]);
        fp.glob_imports = vec!["super".to_string()];
        let caller = "ff_agent::registry::tests::builds_registry";
        // Type re-anchors but the method doesn't exist in the parent module → no edge.
        let types: HashSet<String> = ["ff_agent::registry::NodeRegistry".to_string()].into();
        let no_fns: HashSet<String> = HashSet::new();
        assert_eq!(
            resolve_glob_impl_method_call(
                "ff_agent::registry::tests::NodeRegistry::new",
                &fp,
                caller,
                &no_fns,
                &types,
            ),
            None
        );
        // No glob imports → nothing to re-anchor.
        let plain = fp_with("ff_agent::registry", "ff_agent", &[]);
        let fns: HashSet<String> = ["ff_agent::registry::new".to_string()].into();
        assert_eq!(
            resolve_glob_impl_method_call(
                "ff_agent::registry::tests::NodeRegistry::new",
                &plain,
                caller,
                &fns,
                &types,
            ),
            None
        );
        // The fabricated module must match the caller's module (only re-anchor what
        // resolve_call fabricated into the caller's own namespace).
        assert_eq!(
            resolve_glob_impl_method_call(
                "other::module::NodeRegistry::new",
                &fp,
                caller,
                &fns,
                &types,
            ),
            None
        );
    }

    #[test]
    fn self_method_call_redirects_to_flattened_method() {
        // `Self::is_fresh()` inside `impl PeerMap` resolves to the fabricated
        // extern `ff_pulse::peer_map::Self::is_fresh`; the method is indexed
        // flattened as `ff_pulse::peer_map::is_fresh`. Redirect onto it (the top
        // `ff cortex doctor` Self:: noise source).
        let fns: HashSet<String> = ["ff_pulse::peer_map::is_fresh".to_string()].into();
        assert_eq!(
            resolve_self_method_call("ff_pulse::peer_map::Self::is_fresh", &fns),
            Some("ff_pulse::peer_map::is_fresh".to_string())
        );
    }

    #[test]
    fn self_method_redirect_never_fabricates() {
        // Target method not internal (e.g. an external `Self::deserialize` derive)
        // → keep the extern resolution, invent nothing.
        let empty: HashSet<String> = HashSet::new();
        assert_eq!(
            resolve_self_method_call("ff_pulse::peer_map::Self::deserialize", &empty),
            None
        );
        // No `::Self::` infix → not a Self-call fabrication, left alone.
        let fns: HashSet<String> = ["ff_pulse::peer_map::is_fresh".to_string()].into();
        assert_eq!(
            resolve_self_method_call("ff_pulse::peer_map::is_fresh", &fns),
            None
        );
    }

    #[test]
    fn java_ctor_redirects_to_class_node() {
        // `new AiClient(...)` is recorded as `AiClient::AiClient` and resolved to
        // the extern `com::hireflow360::api::ai::AiClient::AiClient` (no explicit
        // constructor symbol). Redirect onto the class node so instantiation sites
        // show up under `cortex callers AiClient`.
        let types: HashSet<String> = ["com::hireflow360::api::ai::AiClient".to_string()].into();
        let by_leaf = type_leaf_index(&types);
        assert_eq!(
            resolve_java_ctor_call(
                "com::hireflow360::api::ai::AiClient::AiClient",
                &types,
                &by_leaf
            ),
            Some("com::hireflow360::api::ai::AiClient".to_string())
        );
    }

    #[test]
    fn java_ctor_redirect_never_fabricates() {
        // Class not internal (a JDK/3rd-party type, e.g. `new ArrayList()`) → keep
        // the extern, invent nothing.
        let empty: HashSet<String> = HashSet::new();
        let empty_leaf = type_leaf_index(&empty);
        assert_eq!(
            resolve_java_ctor_call("java::util::ArrayList::ArrayList", &empty, &empty_leaf),
            None
        );
        // Not the `::Type::Type` constructor shape (a normal method call) → ignored
        // even when the type is internal.
        let types: HashSet<String> = ["com::hireflow360::api::ai::AiClient".to_string()].into();
        let by_leaf = type_leaf_index(&types);
        assert_eq!(
            resolve_java_ctor_call(
                "com::hireflow360::api::ai::AiClient::send",
                &types,
                &by_leaf
            ),
            None
        );
    }

    #[test]
    fn java_ctor_redirects_nested_class_to_real_outer() {
        // `new Ai()` where Ai is nested in ApiProperties: recorded as the caller's
        // own package path `…config::Ai::Ai`, but the real class lives at
        // `…config::ApiProperties::Ai`. Suffix-match the leaf onto the nested type
        // whose outer prefix (ApiProperties) is itself internal.
        let types: HashSet<String> = [
            "com::hireflow360::api::config::ApiProperties".to_string(),
            "com::hireflow360::api::config::ApiProperties::Ai".to_string(),
        ]
        .into();
        let by_leaf = type_leaf_index(&types);
        assert_eq!(
            resolve_java_ctor_call("com::hireflow360::api::config::Ai::Ai", &types, &by_leaf),
            Some("com::hireflow360::api::config::ApiProperties::Ai".to_string())
        );
    }

    #[test]
    fn java_nested_ctor_ambiguous_stays_extern() {
        // Two distinct nested classes both named `Ai`, each under an internal outer
        // class → ambiguous, must NOT pick one. Leave it extern.
        let types: HashSet<String> = [
            "com::hireflow360::api::config::ApiProperties".to_string(),
            "com::hireflow360::api::config::ApiProperties::Ai".to_string(),
            "com::hireflow360::api::other::OtherProps".to_string(),
            "com::hireflow360::api::other::OtherProps::Ai".to_string(),
        ]
        .into();
        let by_leaf = type_leaf_index(&types);
        assert_eq!(
            resolve_java_ctor_call("com::hireflow360::api::config::Ai::Ai", &types, &by_leaf),
            None
        );
    }

    #[test]
    fn java_nested_ctor_requires_internal_outer() {
        // A nested-named type whose outer prefix is NOT an internal type is not a
        // confirmed nested class (could be a top-level type that merely shares the
        // leaf via a coincidental path) → no redirect.
        let types: HashSet<String> =
            ["com::hireflow360::api::config::Widgets::Ai".to_string()].into();
        let by_leaf = type_leaf_index(&types);
        assert_eq!(
            resolve_java_ctor_call("com::hireflow360::api::config::Ai::Ai", &types, &by_leaf),
            None
        );
    }

    #[test]
    fn java_nested_method_redirects_to_real_outer() {
        // `Message.system(...)` where Message is nested in AiClient: recorded as the
        // caller's own package path `…ai::Message::system`, but the real method node
        // is `…ai::AiClient::Message::system`. Suffix-match the receiver leaf onto the
        // nested type whose outer (AiClient) is internal, then reattach `system`.
        let types: HashSet<String> = [
            "com::hireflow360::api::ai::AiClient".to_string(),
            "com::hireflow360::api::ai::AiClient::Message".to_string(),
        ]
        .into();
        let fns: HashSet<String> =
            ["com::hireflow360::api::ai::AiClient::Message::system".to_string()].into();
        let by_leaf = type_leaf_index(&types);
        assert_eq!(
            resolve_java_method_call(
                "com::hireflow360::api::ai::Message::system",
                &fns,
                &types,
                &by_leaf
            ),
            Some("com::hireflow360::api::ai::AiClient::Message::system".to_string())
        );
    }

    #[test]
    fn java_nested_method_redirect_never_fabricates() {
        let types: HashSet<String> = [
            "com::hireflow360::api::ai::AiClient".to_string(),
            "com::hireflow360::api::ai::AiClient::Message".to_string(),
        ]
        .into();
        let by_leaf = type_leaf_index(&types);
        // Constructor shape (leaf == type_seg) is the ctor resolver's job, not ours.
        assert_eq!(
            resolve_java_method_call(
                "com::hireflow360::api::ai::Message::Message",
                &HashSet::new(),
                &types,
                &by_leaf
            ),
            None
        );
        // Lowercase receiver = an instance variable (`msg.system()`), not a type.
        assert_eq!(
            resolve_java_method_call(
                "com::hireflow360::api::ai::msg::system",
                &HashSet::new(),
                &types,
                &by_leaf
            ),
            None
        );
        // Resolved nested type, but the method isn't a known internal fn → no edge.
        assert_eq!(
            resolve_java_method_call(
                "com::hireflow360::api::ai::Message::nope",
                &HashSet::new(),
                &types,
                &by_leaf
            ),
            None
        );
    }

    #[test]
    fn java_nested_method_ambiguous_stays_extern() {
        // Two nested classes both named `Message`, each under an internal outer →
        // ambiguous receiver, must not guess. Leave it extern.
        let types: HashSet<String> = [
            "com::hireflow360::api::ai::AiClient".to_string(),
            "com::hireflow360::api::ai::AiClient::Message".to_string(),
            "com::hireflow360::api::chat::ChatClient".to_string(),
            "com::hireflow360::api::chat::ChatClient::Message".to_string(),
        ]
        .into();
        let fns: HashSet<String> = [
            "com::hireflow360::api::ai::AiClient::Message::system".to_string(),
            "com::hireflow360::api::chat::ChatClient::Message::system".to_string(),
        ]
        .into();
        let by_leaf = type_leaf_index(&types);
        assert_eq!(
            resolve_java_method_call(
                "com::hireflow360::api::ai::Message::system",
                &fns,
                &types,
                &by_leaf
            ),
            None
        );
    }

    #[test]
    fn java_top_level_type_method_not_redirected() {
        // A top-level class `Foo` (outer prefix is a package, not an internal type)
        // is excluded from the nested filter — a missed cross-package static call is
        // the alias map's job, not this redirect's.
        let types: HashSet<String> = ["com::hireflow360::api::util::Foo".to_string()].into();
        let fns: HashSet<String> = ["com::hireflow360::api::util::Foo::bar".to_string()].into();
        let by_leaf = type_leaf_index(&types);
        assert_eq!(
            resolve_java_method_call(
                "com::hireflow360::api::other::Foo::bar",
                &fns,
                &types,
                &by_leaf
            ),
            None
        );
    }

    #[test]
    fn java_enum_constant_method_redirects_to_declaring_type() {
        // `ErrorCode.INTERNAL_ERROR.status()`: the enum CONSTANT is a value, not a
        // scope, so the real method node is `ErrorCode::status`, not
        // `ErrorCode::INTERNAL_ERROR::status`.
        let types: HashSet<String> =
            ["com::hireflow360::shared::error::ErrorCode".to_string()].into();
        let fns: HashSet<String> =
            ["com::hireflow360::shared::error::ErrorCode::status".to_string()].into();
        assert_eq!(
            resolve_java_member_method_call(
                "com::hireflow360::shared::error::ErrorCode::INTERNAL_ERROR::status",
                &fns,
                &types
            ),
            Some("com::hireflow360::shared::error::ErrorCode::status".to_string())
        );
    }

    #[test]
    fn java_nested_test_class_outer_helper_redirects() {
        // A JUnit `@Nested` inner class calling an OUTER-class helper: the helper
        // lives on the outer test class, not the inner one.
        let types: HashSet<String> =
            ["com::hireflow360::api::connectors::ConnectorFrameworkUnitTests".to_string()].into();
        let fns: HashSet<String> =
            ["com::hireflow360::api::connectors::ConnectorFrameworkUnitTests::apiEx".to_string()]
                .into();
        assert_eq!(
            resolve_java_member_method_call(
                "com::hireflow360::api::connectors::ConnectorFrameworkUnitTests::DisabledByDefaultContract::apiEx",
                &fns,
                &types
            ),
            Some(
                "com::hireflow360::api::connectors::ConnectorFrameworkUnitTests::apiEx".to_string()
            )
        );
    }

    #[test]
    fn java_member_method_redirect_never_fabricates() {
        let types: HashSet<String> =
            ["com::hireflow360::shared::error::ErrorCode".to_string()].into();
        // Owner is a known type but the dropped-middle target isn't a known fn.
        assert_eq!(
            resolve_java_member_method_call(
                "com::hireflow360::shared::error::ErrorCode::INTERNAL_ERROR::nope",
                &HashSet::new(),
                &types
            ),
            None
        );
        // Owner isn't a known internal type → no redirect (e.g. a String constant
        // calling Object.equals: `…admin::DEMO_ORGANIZATION_ID::equals`).
        let fns: HashSet<String> = ["com::hireflow360::api::admin::equals".to_string()].into();
        assert_eq!(
            resolve_java_member_method_call(
                "com::hireflow360::api::admin::DEMO_ORGANIZATION_ID::equals",
                &fns,
                &HashSet::new()
            ),
            None
        );
    }

    #[test]
    fn java_member_method_lowercase_mid_not_redirected() {
        // A fluent method chain `builder.build().toString()` records
        // `Builder::build::toString`; `build` is lowerCamel (a method), so the
        // result of `build()` must NOT redirect onto `Builder::toString`.
        let types: HashSet<String> = ["com::hireflow360::api::util::Builder".to_string()].into();
        let fns: HashSet<String> =
            ["com::hireflow360::api::util::Builder::toString".to_string()].into();
        assert_eq!(
            resolve_java_member_method_call(
                "com::hireflow360::api::util::Builder::build::toString",
                &fns,
                &types
            ),
            None
        );
    }

    #[test]
    fn java_member_method_requires_module_qualified_owner() {
        // A bare two-segment owner (`Type::CONST::leaf`) can't redirect — the owner
        // must carry a package so an accidental match is impossible.
        let types: HashSet<String> = ["ErrorCode".to_string()].into();
        let fns: HashSet<String> = ["ErrorCode::status".to_string()].into();
        assert_eq!(
            resolve_java_member_method_call("ErrorCode::INTERNAL_ERROR::status", &fns, &types),
            None
        );
    }

    #[test]
    fn java_lang_prelude_call_not_caller_pkg_rooted() {
        // `Boolean.TRUE.equals(x)` in a Java file: head `Boolean` is java.lang,
        // not imported. Must resolve to a shared `java::lang::…` extern, NOT to
        // `<caller_pkg>::Boolean::TRUE::equals` (whose `equals` leaf collides with
        // internal methods — the top `ff cortex doctor` Java noise source).
        let mut fp = fp_with("com::hireflow360::api::vetting", "com", &[]);
        fp.lang = Lang::Java;
        assert_eq!(
            resolve_call(
                "Boolean::TRUE::equals",
                "com::hireflow360::api::vetting::run",
                &fp
            ),
            "java::lang::Boolean::TRUE::equals"
        );
        // java.util static holder too.
        assert_eq!(
            resolve_call(
                "Objects::requireNonNull",
                "com::hireflow360::api::vetting::run",
                &fp
            ),
            "java::util::Objects::requireNonNull"
        );
        // A non-prelude uppercase head with no import keeps the same-package
        // behavior (could be a constructor / static call on a local type).
        assert_eq!(
            resolve_call(
                "MyService::create",
                "com::hireflow360::api::vetting::run",
                &fp
            ),
            "com::hireflow360::api::vetting::MyService::create"
        );
    }

    #[test]
    fn glob_super_redirects_bare_call_to_parent_module() {
        // The documented dogfood bug: `mod tests { use super::*; }` then a bare
        // `should_compact(` inside a test. resolve_call alone attributes it to the
        // tests submodule (no local def, not aliased); the glob fallback must
        // redirect it to the parent module's real fn.
        let mut fp = fp_with("ff_agent::compaction", "ff_agent", &[]);
        fp.glob_imports = vec!["super".to_string()];
        let caller = "ff_agent::compaction::tests::compacts_when_oversized";
        // Plain resolution points at the (phantom) tests-submodule fn.
        assert_eq!(
            resolve_call("should_compact", caller, &fp),
            "ff_agent::compaction::tests::should_compact"
        );
        // With the parent fn known internal, the glob fallback corrects it.
        let internal: HashSet<String> = ["ff_agent::compaction::should_compact".to_string()].into();
        assert_eq!(
            resolve_glob_call("should_compact", caller, &fp, &internal),
            Some("ff_agent::compaction::should_compact".to_string())
        );
    }

    #[test]
    fn glob_fallback_does_not_fabricate_when_no_real_symbol() {
        // No internal symbol matches → no redirect (the call keeps its extern
        // resolution). Prevents glob imports from inventing edges.
        let mut fp = fp_with("ff_agent::compaction", "ff_agent", &[]);
        fp.glob_imports = vec!["super".to_string()];
        let internal: HashSet<String> = HashSet::new();
        assert_eq!(
            resolve_glob_call(
                "should_compact",
                "ff_agent::compaction::tests::t",
                &fp,
                &internal
            ),
            None
        );
    }

    #[test]
    fn glob_absolute_prefix_resolves_bare_call() {
        // `use crate::util::*;` (stored absolute as ff_agent::util) brings `helper`
        // into scope at the file's top module.
        let mut fp = fp_with("ff_agent::worker", "ff_agent", &[]);
        fp.glob_imports = vec!["ff_agent::util".to_string()];
        let internal: HashSet<String> = ["ff_agent::util::helper".to_string()].into();
        assert_eq!(
            resolve_glob_call("helper", "ff_agent::worker::run", &fp, &internal),
            Some("ff_agent::util::helper".to_string())
        );
    }

    #[test]
    fn glob_fallback_skips_aliased_and_qualified_calls() {
        // An explicit `use` (alias) wins over the glob; a `::`-qualified call is
        // never a glob candidate.
        let mut fp = fp_with(
            "ff_agent::worker",
            "ff_agent",
            &[("helper", "ff_agent::explicit::helper")],
        );
        fp.glob_imports = vec!["super".to_string()];
        let internal: HashSet<String> = ["ff_agent::worker::helper".to_string()].into();
        // aliased bare ident: fallback declines (alias already resolved it).
        assert_eq!(
            resolve_glob_call("helper", "ff_agent::worker::tests::t", &fp, &internal),
            None
        );
        // qualified call: not a glob candidate.
        assert_eq!(
            resolve_glob_call("a::b", "ff_agent::worker::tests::t", &fp, &internal),
            None
        );
    }

    #[test]
    fn glob_prefix_super_super_pops_two() {
        assert_eq!(
            resolve_glob_prefix("super", "a::b::tests"),
            Some("a::b".to_string())
        );
        assert_eq!(
            resolve_glob_prefix("super::super", "a::b::c::tests"),
            Some("a::b".to_string())
        );
        assert_eq!(
            resolve_glob_prefix("self::inner", "a::b"),
            Some("a::b::inner".to_string())
        );
        // already-absolute prefix is returned unchanged.
        assert_eq!(
            resolve_glob_prefix("x::y", "a::b"),
            Some("x::y".to_string())
        );
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
    fn anchor_at_crate_root_strips_caller_prefix() {
        let roots: HashSet<String> = ["ff_db".to_string(), "ff_gateway".to_string()].into();
        // caller-prefixed crate-qualified path -> re-anchored at the crate root.
        assert_eq!(
            anchor_at_crate_root("ff_gateway::brain_api::ff_db::pg_get_brain_user", &roots),
            Some("ff_db::pg_get_brain_user".to_string())
        );
        // head is already a crate root (position 0) -> nothing to strip.
        assert_eq!(
            anchor_at_crate_root("ff_db::run_postgres_migrations", &roots),
            None
        );
        // no crate root anywhere -> None.
        assert_eq!(anchor_at_crate_root("some::ext::thing", &roots), None);
    }

    #[test]
    fn reexport_target_recovers_cross_crate_misprefix() {
        // The load-time recovery (anchor_at_crate_root over every re-export target)
        // unbreaks a cross-crate `pub use` the per-file parser mis-prefixed.
        let roots: HashSet<String> = [
            "ff_core".to_string(),
            "ff_db".to_string(),
            "ff_gateway".to_string(),
        ]
        .into();
        // `pub(crate) use ff_core::model_id::normalize_model_id;` in
        // `ff_gateway::llm_routing` is stored as a submodule path; recover it.
        assert_eq!(
            anchor_at_crate_root(
                "ff_gateway::llm_routing::ff_core::model_id::normalize_model_id",
                &roots,
            ),
            Some("ff_core::model_id::normalize_model_id".to_string())
        );
        // glob form `pub use ff_core::model_id::*;` -> the target MODULE recovers.
        assert_eq!(
            anchor_at_crate_root("ff_gateway::llm_routing::ff_core::model_id", &roots),
            Some("ff_core::model_id".to_string())
        );
        // a GENUINE submodule re-export (`pub use leader_state::*;` in ff_db) has no
        // inner crate root -> left untouched, so `ff_db::leader_state` survives.
        assert_eq!(anchor_at_crate_root("ff_db::leader_state", &roots), None);
        // a correctly-stored cross-crate target (crate at head) is already absolute.
        assert_eq!(
            anchor_at_crate_root("ff_core::model_id::normalize_model_id", &roots),
            None
        );
    }

    #[test]
    fn chase_reexport_follows_chain_to_internal() {
        let map: HashMap<String, String> = [
            (
                "ff_db::run_postgres_migrations".to_string(),
                "ff_db::a::run".to_string(),
            ),
            (
                "ff_db::a::run".to_string(),
                "ff_db::migrations::run_postgres_migrations".to_string(),
            ),
        ]
        .into();
        let fns: HashSet<String> =
            ["ff_db::migrations::run_postgres_migrations".to_string()].into();
        assert_eq!(
            chase_reexport("ff_db::run_postgres_migrations", &map, &fns),
            Some("ff_db::migrations::run_postgres_migrations".to_string())
        );
        // a facade that never lands internal -> None.
        let empty: HashSet<String> = HashSet::new();
        assert_eq!(
            chase_reexport("ff_db::run_postgres_migrations", &map, &empty),
            None
        );
    }

    #[test]
    fn facade_redirect_resolves_function_reexport() {
        // ff_db re-exports `run_postgres_migrations` at the crate root; the real fn lives in
        // ff_db::migrations. Doctor hit: `ff_db::run_postgres_migrations` fan-in 14.
        let reexports: HashMap<String, String> = [(
            "ff_db::run_postgres_migrations".to_string(),
            "ff_db::migrations::run_postgres_migrations".to_string(),
        )]
        .into();
        let roots: HashSet<String> = ["ff_db".to_string()].into();
        let fns: HashSet<String> =
            ["ff_db::migrations::run_postgres_migrations".to_string()].into();
        let types: HashSet<String> = HashSet::new();
        assert_eq!(
            resolve_facade_call(
                "ff_db::run_postgres_migrations",
                &reexports,
                &HashMap::new(),
                &roots,
                &fns,
                &types
            ),
            Some("ff_db::migrations::run_postgres_migrations".to_string())
        );
    }

    #[test]
    fn facade_redirect_resolves_glob_reexport() {
        // ff_db re-exports via a GLOB `pub use leader_state::*;`, so
        // `ff_db::pg_get_current_leader` aliases `ff_db::leader_state::pg_get_current_leader`.
        // The leaf set is unknown at parse time, so this can't go in the named
        // reexport map — the glob map probes the target module for the leaf.
        // Doctor hit: `ff_terminal::fleet_cmd::ff_db::pg_get_current_leader` (#6).
        let reexports: HashMap<String, String> = HashMap::new();
        let globs: HashMap<String, Vec<String>> =
            [("ff_db".to_string(), vec!["ff_db::leader_state".to_string()])].into();
        let roots: HashSet<String> = ["ff_db".to_string(), "ff_terminal".to_string()].into();
        let fns: HashSet<String> =
            ["ff_db::leader_state::pg_get_current_leader".to_string()].into();
        let types: HashSet<String> = HashSet::new();
        // Caller-prefixed onto the crate-rooted glob-facade path: #4 anchor + #6 glob.
        assert_eq!(
            resolve_facade_call(
                "ff_terminal::fleet_cmd::ff_db::pg_get_current_leader",
                &reexports,
                &globs,
                &roots,
                &fns,
                &types
            ),
            Some("ff_db::leader_state::pg_get_current_leader".to_string())
        );
        // A glob facade for a leaf that ISN'T in the target module stays extern.
        assert_eq!(
            resolve_facade_call(
                "ff_db::not_a_real_fn",
                &reexports,
                &globs,
                &roots,
                &fns,
                &types
            ),
            None
        );
    }

    #[test]
    fn facade_redirect_resolves_caller_prefixed_reexport() {
        // Doctor hit: `ff_gateway::brain_api::ff_db::pg_get_brain_user` fan-in 13.
        // Caller-prefixed (#4) AND a facade re-export (#3) — both must compose.
        let reexports: HashMap<String, String> = [(
            "ff_db::pg_get_brain_user".to_string(),
            "ff_db::queries::pg_get_brain_user".to_string(),
        )]
        .into();
        let roots: HashSet<String> = ["ff_db".to_string(), "ff_gateway".to_string()].into();
        let fns: HashSet<String> = ["ff_db::queries::pg_get_brain_user".to_string()].into();
        let types: HashSet<String> = HashSet::new();
        assert_eq!(
            resolve_facade_call(
                "ff_gateway::brain_api::ff_db::pg_get_brain_user",
                &reexports,
                &HashMap::new(),
                &roots,
                &fns,
                &types
            ),
            Some("ff_db::queries::pg_get_brain_user".to_string())
        );
    }

    #[test]
    fn facade_redirect_resolves_crate_root_anchor_without_facade() {
        // #4 alone: caller-prefixed onto a crate-rooted path whose target is the
        // real fn directly (no `pub use` facade needed).
        let reexports: HashMap<String, String> = HashMap::new();
        let roots: HashSet<String> = ["ff_db".to_string(), "ff_gateway".to_string()].into();
        let fns: HashSet<String> = ["ff_db::queries::pg_list_nodes".to_string()].into();
        let types: HashSet<String> = HashSet::new();
        assert_eq!(
            resolve_facade_call(
                "ff_gateway::server::ff_db::queries::pg_list_nodes",
                &reexports,
                &HashMap::new(),
                &roots,
                &fns,
                &types
            ),
            Some("ff_db::queries::pg_list_nodes".to_string())
        );
    }

    #[test]
    fn facade_redirect_resolves_type_facade_method() {
        // Doctor hit: `ff_db::OperationalStore::postgres` fan-in 14. The TYPE is re-exported
        // (`ff_db::OperationalStore` -> `ff_db::operational_store::OperationalStore`); the method is indexed
        // flattened at the type's real module (`ff_db::operational_store::postgres`).
        let reexports: HashMap<String, String> = [(
            "ff_db::OperationalStore".to_string(),
            "ff_db::operational_store::OperationalStore".to_string(),
        )]
        .into();
        let roots: HashSet<String> = ["ff_db".to_string()].into();
        let fns: HashSet<String> = ["ff_db::operational_store::postgres".to_string()].into();
        let types: HashSet<String> =
            ["ff_db::operational_store::OperationalStore".to_string()].into();
        assert_eq!(
            resolve_facade_call(
                "ff_db::OperationalStore::postgres",
                &reexports,
                &HashMap::new(),
                &roots,
                &fns,
                &types
            ),
            Some("ff_db::operational_store::postgres".to_string())
        );
    }

    #[test]
    fn facade_redirect_never_fabricates() {
        // A genuine external call must stay extern: no facade, no internal target.
        let reexports: HashMap<String, String> = HashMap::new();
        let roots: HashSet<String> = ["ff_db".to_string()].into();
        let fns: HashSet<String> = HashSet::new();
        let types: HashSet<String> = HashSet::new();
        assert_eq!(
            resolve_facade_call(
                "toml::from_str",
                &reexports,
                &HashMap::new(),
                &roots,
                &fns,
                &types
            ),
            None
        );
    }

    #[test]
    fn abs_reexport_target_resolves_relative_forms() {
        // bare submodule path is relative to the re-exporting module.
        assert_eq!(
            abs_reexport_target("operational_store::OperationalStore", "ff_db", "ff_db"),
            "ff_db::operational_store::OperationalStore"
        );
        // crate-rooted path (norm_crate already rewrote `crate::`) is absolute.
        assert_eq!(
            abs_reexport_target("ff_db::queries::pg_get_brain_user", "ff_db", "ff_db"),
            "ff_db::queries::pg_get_brain_user"
        );
        // super:: is relative to the re-exporting module.
        assert_eq!(
            abs_reexport_target("super::inner::X", "ff_db::facade", "ff_db"),
            "ff_db::inner::X"
        );
    }

    #[test]
    fn record_reexport_skips_private_and_self_alias() {
        let mut fp = fp_with("ff_db", "ff_db", &[]);
        // private use (base None) records nothing.
        record_reexport(None, "X", "ff_db::a::X", "ff_db", &mut fp);
        assert!(fp.reexports.is_empty());
        // re-export of a same-name same-module item (facade == target) is a no-op.
        record_reexport(Some("ff_db"), "X", "ff_db::X", "ff_db", &mut fp);
        assert!(fp.reexports.is_empty());
        // genuine re-export is recorded with an absolute target.
        record_reexport(
            Some("ff_db"),
            "run_postgres_migrations",
            "migrations::run_postgres_migrations",
            "ff_db",
            &mut fp,
        );
        assert_eq!(
            fp.reexports,
            vec![(
                "ff_db::run_postgres_migrations".to_string(),
                "ff_db::migrations::run_postgres_migrations".to_string()
            )]
        );
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
    fn third_party_crate_call_not_caller_prefixed() {
        // `toml::from_str(...)` / `dirs::home_dir()` written bare must stay a
        // shared extern, not a `<caller_module>::toml::from_str` phantom.
        let fp = fp_with("ff_core::hot_reload::tests", "ff_core", &[]);
        assert_eq!(
            resolve_call("toml::from_str", "ff_core::hot_reload::tests::t", &fp),
            "toml::from_str"
        );
        assert_eq!(
            resolve_call("dirs::home_dir", "ff_agent::shared_workspace::f", &fp),
            "dirs::home_dir"
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
        // bare load_model resolves to the IMPORTED fn via the alias map, not a
        // fabricated ff_agent::autoscaler::load_model. With `use ...::load_model`
        // in scope, a local `load_model` would be a name conflict in real Rust,
        // so the import is the unambiguous callee.
        assert_eq!(got, "ff_agent::model_runtime::load_model");
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
    fn parse_records_pub_use_reexport() {
        // A facade crate root: `pub use` makes the submodule item visible at the
        // crate root (recorded as a re-export); a private `use` does not.
        let src = "pub use crate::migrations::run_postgres_migrations;\n\
                   use crate::other::helper;\n\
                   pub fn a() {}\n";
        let fp = parse_rust_file("/x/crates/ff_db/src/lib.rs", src).unwrap();
        assert!(
            fp.reexports.iter().any(|(facade, target)| facade
                .ends_with("::run_postgres_migrations")
                && target.ends_with("::migrations::run_postgres_migrations")),
            "pub use not recorded: {:?}",
            fp.reexports
        );
        assert!(
            !fp.reexports
                .iter()
                .any(|(_, t)| t.ends_with("::other::helper")),
            "private use must not re-export: {:?}",
            fp.reexports
        );
    }

    #[test]
    fn parse_records_pub_use_glob_reexport() {
        // A glob `pub use m::*;` re-exports every item of `m` at the crate root,
        // recorded as a (facade_base, target_module) glob re-export; a private
        // glob `use m::*;` only brings names into local scope, not re-exported.
        // Use the real-world RELATIVE form (`pub use leader_state::*;`, no
        // `crate::`) — the target must be made absolute relative to the
        // re-exporting module (base), else the redirect probes the wrong module.
        let src = "pub use leader_state::*;\n\
                   use internal::*;\n\
                   pub fn a() {}\n";
        // No Cargo.toml on disk, so the crate root falls back to `crate` (same as
        // the named-reexport test); real indexing resolves it to `ff_db`. Assert on
        // shape: target == base::leader_state (absolute, base-relative).
        let fp = parse_rust_file("/x/crates/ff_db/src/lib.rs", src).unwrap();
        assert!(
            fp.glob_reexports
                .iter()
                .any(|(base, target)| *target == format!("{base}::leader_state")),
            "pub use glob not recorded absolute: {:?}",
            fp.glob_reexports
        );
        assert!(
            !fp.glob_reexports
                .iter()
                .any(|(_, t)| t.ends_with("::internal")),
            "private use glob must not re-export: {:?}",
            fp.glob_reexports
        );
    }

    #[test]
    fn parse_extracts_calls_inside_macro_bodies() {
        // The Rust call-graph blind spot: calls inside macro invocations
        // (`assert_eq!`, `assert!`, …) live in an opaque token_tree and used to
        // yield NO call edge — so macro-asserted unit tests read as covering
        // nothing and `ff cortex tests` under-reported Rust coverage. The macro
        // name itself must NOT be recorded, nor method calls.
        let src = r#"
fn target(x: u32) -> u32 { x + 1 }
fn other() -> u32 { 7 }
struct S;
impl S { fn meth(&self) -> u32 { 1 } }
#[test]
fn covers_target() {
    let s = S;
    assert_eq!(target(2), 3);
    assert!(other() > 0);
    println!("{}", s.meth());
}
"#;
        let fp = parse_rust_file("/x/crates/demo/src/lib.rs", src).unwrap();
        let raws: Vec<&str> = fp.calls.iter().map(|c| c.raw_path.as_str()).collect();
        // The fns invoked from inside the macros are now captured as calls.
        assert!(
            raws.contains(&"target"),
            "macro-body call to target missing: {raws:?}"
        );
        assert!(
            raws.contains(&"other"),
            "macro-body call to other missing: {raws:?}"
        );
        // The macro names are NOT calls (the `!` separates name from token_tree).
        assert!(
            !raws.contains(&"assert_eq"),
            "macro name leaked as call: {raws:?}"
        );
        assert!(
            !raws.contains(&"assert"),
            "macro name leaked as call: {raws:?}"
        );
        assert!(
            !raws.contains(&"println"),
            "macro name leaked as call: {raws:?}"
        );
        // Method call inside the macro is skipped (matches call_expression path).
        assert!(
            !raws.contains(&"meth"),
            "method call wrongly recorded: {raws:?}"
        );
    }

    #[test]
    fn parse_rebuilds_scoped_call_inside_macro_body() {
        // A scoped associated call `Type::method(args)` inside a macro body
        // (`vec![…]`, `assert_eq!`, …) is lexed as the flat tokens
        // `Type :: method ( … )`, so the trailing `method` identifier alone used
        // to be recorded as a bare call — fabricating `<caller_module>::method`
        // instead of `Type::method` (the real, flattened impl method). Mirrors the
        // `ToolChatMessage::system(…)` calls in compaction.rs's test `vec![…]`.
        let src = r#"
struct Msg;
impl Msg {
    fn system(s: &str) -> Msg { Msg }
    fn user(s: &str) -> Msg { Msg }
}
fn free(x: u32) -> u32 { x }
mod inner { pub mod deep { pub fn f() -> u32 { 1 } } }
#[test]
fn covers() {
    let msgs = vec![Msg::system("a"), Msg::user("b")];
    assert_eq!(inner::deep::f(), 1);
    assert!(free(2) > 0);
}
"#;
        let fp = parse_rust_file("/x/crates/demo/src/lib.rs", src).unwrap();
        let raws: Vec<&str> = fp.calls.iter().map(|c| c.raw_path.as_str()).collect();
        // Scoped associated calls keep their type/path segment.
        assert!(
            raws.contains(&"Msg::system"),
            "scoped macro-body call lost its type segment: {raws:?}"
        );
        assert!(
            raws.contains(&"Msg::user"),
            "scoped macro-body call lost its type segment: {raws:?}"
        );
        // Multi-segment module path is rebuilt whole.
        assert!(
            raws.contains(&"inner::deep::f"),
            "multi-segment macro-body call not rebuilt: {raws:?}"
        );
        // A genuine bare call is unchanged.
        assert!(
            raws.contains(&"free"),
            "bare macro-body call missing: {raws:?}"
        );
        // The bare-leaf fabrications must NOT appear.
        assert!(
            !raws.contains(&"system") && !raws.contains(&"user"),
            "bare-leaf phantom still recorded: {raws:?}"
        );
    }

    #[test]
    fn parse_rebuilds_crate_self_super_call_head_inside_macro_body() {
        // `crate::`/`self::`/`super::` are path ROOTS lexed as their own keyword
        // nodes inside a macro token_tree — NOT `identifier`. The macro-recovery
        // walk-back used to stop at them, collapsing `crate::foo(…)` to a bare
        // `foo` and fabricating `<caller_module>::foo`. Real-world hit:
        // `crate::detect_code_communities(&pg)` inside a `tokio::select!` arm
        // resolved to the phantom `ff_brain::community_summary::detect_code_communities`.
        let src = r#"
mod a { pub fn foo() -> u32 { 1 } }
mod b {
    pub fn bar() -> u32 { 2 }
    pub mod c {
        pub fn baz() -> u32 { 3 }
        pub fn run() -> u32 {
            assert!(crate::a::foo() > 0);
            assert!(self::baz() > 0);
            assert!(super::bar() > 0);
            0
        }
    }
}
"#;
        let fp = parse_rust_file("/x/crates/demo/src/lib.rs", src).unwrap();
        let raws: Vec<&str> = fp.calls.iter().map(|c| c.raw_path.as_str()).collect();
        // Each path-rooted call keeps its full head inside the macro body.
        assert!(
            raws.contains(&"crate::a::foo"),
            "crate:: head lost inside macro body: {raws:?}"
        );
        assert!(
            raws.contains(&"self::baz"),
            "self:: head lost inside macro body: {raws:?}"
        );
        assert!(
            raws.contains(&"super::bar"),
            "super:: head lost inside macro body: {raws:?}"
        );
        // The bare-leaf fabrications must NOT appear.
        assert!(
            !raws.contains(&"foo") && !raws.contains(&"baz") && !raws.contains(&"bar"),
            "bare-leaf phantom still recorded: {raws:?}"
        );
    }

    #[test]
    fn parse_drops_prelude_variant_constructors() {
        // `Ok`/`Err`/`Some`/`None` parse as bare-identifier call_expressions and
        // used to be fabricated into a phantom `<caller_module>::Ok` extern with a
        // `calls` edge — ~19% of forge-fleet's externs and the top hit of
        // `ff cortex find Ok`. They appear in nearly every fn and never resolve to
        // an internal symbol, so they are dropped at collection. Real calls in the
        // same body (including inside macro bodies) must still be recorded.
        let src = r#"
fn helper(x: u32) -> u32 { x }
fn run() -> Result<u32, String> {
    let v = helper(1);
    if v == 0 {
        return Err("zero".into());
    }
    let _ = Some(v);
    assert_eq!(helper(2), 2);
    Ok(v)
}
"#;
        let fp = parse_rust_file("/x/crates/demo/src/lib.rs", src).unwrap();
        let raws: Vec<&str> = fp.calls.iter().map(|c| c.raw_path.as_str()).collect();
        for ctor in ["Ok", "Err", "Some", "None"] {
            assert!(
                !raws.contains(&ctor),
                "prelude constructor {ctor} leaked as a call: {raws:?}"
            );
        }
        // Real calls — bare and macro-body — are still captured.
        assert!(
            raws.iter().filter(|r| **r == "helper").count() >= 2,
            "real call to helper missing (bare + macro-body): {raws:?}"
        );
    }

    #[test]
    fn parse_does_not_record_attribute_args_as_calls() {
        // Attribute arguments parse as `identifier (token_tree)` — the exact shape
        // the macro-body call recovery keys on — so `#[derive(Debug)]`,
        // `#[cfg(test)]`, `#[serde(rename = "x")]`, `#[arg(long)]` etc. inside a fn
        // body used to be recorded as phantom calls (`<caller_module>::derive`,
        // `::cfg`, `::serde`…), polluting the extern set (see `ff cortex doctor`'s
        // `<module>::derive` cluster). Attributes are metadata, never call edges.
        let src = r#"
fn helper(x: u32) -> u32 { x }
fn run() -> u32 {
    #[derive(Debug, Clone, serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    struct Local {
        #[serde(default)]
        a: u32,
    }
    #[cfg(test)]
    fn only_in_test() {}
    helper(1)
}
"#;
        let fp = parse_rust_file("/x/crates/demo/src/lib.rs", src).unwrap();
        let raws: Vec<&str> = fp.calls.iter().map(|c| c.raw_path.as_str()).collect();
        for attr in ["derive", "cfg", "serde", "serde::Serialize"] {
            assert!(
                !raws.contains(&attr),
                "attribute arg {attr} leaked as a call: {raws:?}"
            );
        }
        // The genuine call in the same body is still captured.
        assert!(
            raws.contains(&"helper"),
            "real call to helper missing: {raws:?}"
        );
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
        // the `import java.util.*;` package wildcard is stashed as a redirect base
        // (no alias leaf), so a bare wildcard-imported call can resolve later.
        assert!(fp.wildcard_imports.iter().any(|t| t == "java::util"));
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
    fn cross_corpus_edge_cols_direction() {
        // callers: the matched node is the call SOURCE; the def is the dst.
        assert_eq!(
            super::cross_corpus_edge_cols(true),
            ("e.src_id", "e.dst_id")
        );
        // callees: reversed — the def is the src, the matched node is the dst.
        assert_eq!(
            super::cross_corpus_edge_cols(false),
            ("e.dst_id", "e.src_id")
        );
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
