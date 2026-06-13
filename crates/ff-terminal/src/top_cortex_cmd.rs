//! Top-level `ff cortex …` — a graphify-grade ergonomic wrapper around the
//! `ff brain corpus add` + `ff brain cortex index` two-step. It reuses the exact
//! same graph logic (ff_brain::corpus + ff_brain::cortex) and the existing
//! brain cortex handler, so behavior is identical to the long form.

use crate::{CYAN, GREEN, RESET, YELLOW};
use anyhow::{Result, anyhow};
use clap::Subcommand;
use ff_brain::{corpus, cortex};
use sqlx::PgPool;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, clap::Args)]
pub struct TopCortexArgs {
    #[command(subcommand)]
    pub command: TopCortexCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum TopCortexCommand {
    /// One-shot: create-or-reuse a corpus from PATH (default: cwd), auto-detect
    /// language(s) from the files present, and index the code graph.
    Index {
        /// Directory to index (default: current directory).
        path: Option<String>,
        /// Override the auto-derived corpus slug.
        #[arg(long)]
        slug: Option<String>,
        /// Force a specific language instead of auto-detecting (rust/typescript/javascript/java).
        #[arg(long)]
        lang: Option<String>,
        /// Reindex only files changed since the last index (skip the full rewipe).
        /// First run on a corpus reindexes everything; later runs touch only diffs.
        #[arg(long)]
        incremental: bool,
    },
    /// Show the indexed corpus for the cwd (or --all corpora): node/edge counts.
    Status {
        /// Show all corpora, not just the one matching the cwd.
        #[arg(long)]
        all: bool,
        /// Override which corpus slug to look up.
        #[arg(long)]
        slug: Option<String>,
        #[arg(long, default_value = "table")]
        format: String,
    },
    /// Callers of a code symbol (corpus defaults to the cwd's slug).
    Callers {
        symbol: String,
        #[arg(long)]
        corpus: Option<String>,
        #[arg(long, default_value = "table")]
        format: String,
    },
    /// Callees of a code symbol (corpus defaults to the cwd's slug).
    Callees {
        symbol: String,
        #[arg(long)]
        corpus: Option<String>,
        #[arg(long, default_value = "table")]
        format: String,
    },
    /// Find code symbols by name (case-insensitive substring), ranked by fan-in
    /// — the discovery entrypoint: locate a symbol, then drill in with
    /// callers/callees/impact. Corpus defaults to the cwd's slug.
    Find {
        /// Substring to match against symbol qualified names (case-insensitive),
        /// or — with --semantic — a natural-language intent ("where we publish heartbeats").
        query: String,
        #[arg(long)]
        corpus: Option<String>,
        /// Rank by embedding similarity (bge-m3) instead of name substring — finds
        /// symbols by INTENT when you don't know the name. Requires `ff cortex embed`
        /// to have run and a live fleet embedding endpoint.
        #[arg(long)]
        semantic: bool,
        /// Max hits to return (1-500).
        #[arg(long, default_value_t = 20)]
        limit: i64,
        #[arg(long, default_value = "table")]
        format: String,
    },
    /// Transitive caller closure / blast radius (corpus defaults to the cwd's slug).
    Impact {
        symbol: String,
        #[arg(long)]
        corpus: Option<String>,
        #[arg(long, default_value_t = 5)]
        max_depth: usize,
        #[arg(long, default_value = "table")]
        format: String,
    },
    /// Change-aware, risk-scored review map of the current diff: which changed
    /// symbols have the widest blast radius (fan-in / transitive callers), so a
    /// reviewer knows where to look first. Reads `git diff` for the changed
    /// files, then scores them against the Cortex graph.
    Review {
        /// Compare against this base ref (e.g. `main`) — reviews the branch's
        /// own commits PLUS any uncommitted edits. Default: uncommitted changes
        /// (staged + unstaged + untracked) vs HEAD.
        #[arg(long)]
        base: Option<String>,
        /// Repo directory (default: current directory).
        path: Option<String>,
        /// Override the corpus slug (default: derived from the directory).
        #[arg(long)]
        corpus: Option<String>,
        /// Transitive blast-radius depth.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        #[arg(long, default_value = "table")]
        format: String,
    },
    /// Semantic-embed the Cortex graph: fill the `embedding` column on every
    /// code/doc/data/image node via the fleet's bge-m3 endpoint so semantic
    /// search (`ff brain search`) works over Cortex, then (unless --no-community)
    /// run community detection. Resumable — only NULL nodes are embedded.
    Embed {
        /// Cap nodes processed this run (default: all unembedded).
        #[arg(long)]
        max: Option<usize>,
        /// Embed only this corpus slug first. The fleet-wide pass embeds by
        /// `updated_at`, so a freshly-reindexed repo is embedded LAST — scope to
        /// its slug to make `ff cortex find --semantic` work on it immediately.
        #[arg(long)]
        corpus: Option<String>,
        /// Skip the community-detection pass after embedding.
        #[arg(long)]
        no_community: bool,
    },
    /// Generate per-community natural-language summaries via a fleet LLM and
    /// store them on the brain_communities registry (cortex roadmap #4). Run
    /// after `ff cortex embed` (which detects communities). By default only
    /// summarizes communities with no summary yet; stable member-hash identity
    /// means an unchanged community keeps its summary across re-detection.
    Summarize {
        /// Re-summarize every eligible community, not just un-summarized ones.
        #[arg(long)]
        all: bool,
        /// Cap communities processed this run (unattended quality is hard —
        /// start small and inspect the samples).
        #[arg(long, default_value_t = 20)]
        max: usize,
        /// Skip communities with fewer than this many members.
        #[arg(long, default_value_t = 3)]
        min_members: usize,
        /// Override the fleet endpoint, e.g. http://192.168.5.100:55001 (a
        /// known-good synthesizer). Default: DB-routed warm tool-capable endpoint.
        #[arg(long)]
        llm: Option<String>,
        /// Model id to send with --llm (ignored when DB-routed).
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value = "table")]
        format: String,
    },
    /// Manage the git post-commit hook that re-indexes after every commit.
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
    /// Foreground watcher: re-index whenever files under PATH change.
    Watch {
        /// Directory to watch + index (default: current directory).
        path: Option<String>,
        /// Override the auto-derived corpus slug.
        #[arg(long)]
        slug: Option<String>,
        /// Force a specific language instead of auto-detecting.
        #[arg(long)]
        lang: Option<String>,
        /// Debounce window in seconds: collapse bursts of edits into one re-index.
        #[arg(long, default_value_t = 3)]
        debounce: u64,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum HookAction {
    /// Install a `post-commit` hook in PATH (default cwd) that runs `ff cortex index`.
    Install {
        /// Repo directory whose .git/hooks the hook is written into (default: cwd).
        path: Option<String>,
    },
    /// Remove the ForgeFleet block from PATH's `post-commit` hook (default cwd).
    Uninstall { path: Option<String> },
}

const HOOK_BEGIN: &str = "# >>> ff cortex auto-index >>>";
const HOOK_END: &str = "# <<< ff cortex auto-index <<<";

/// Sanitize a directory basename into a corpus slug (lowercase, alnum + dashes).
fn slug_from_path(p: &Path) -> String {
    let base = p
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("corpus")
        .to_lowercase();
    let mut out = String::with_capacity(base.len());
    let mut prev_dash = false;
    for ch in base.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() {
        "corpus".to_string()
    } else {
        s
    }
}

/// Walk the tree (bounded) counting source files per language so we can pick the
/// dominant one(s). Skips the usual heavy / vendored dirs.
fn detect_languages(root: &Path) -> Vec<(String, usize)> {
    use std::collections::HashMap;
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    let skip = |name: &str| {
        matches!(
            name,
            ".git"
                | "target"
                | "node_modules"
                | "dist"
                | "build"
                | ".venv"
                | "venv"
                | "__pycache__"
                | ".next"
                | "vendor"
        )
    };
    let mut stack = vec![root.to_path_buf()];
    let mut visited_dirs = 0usize;
    while let Some(dir) = stack.pop() {
        if visited_dirs > 50_000 {
            break;
        }
        visited_dirs += 1;
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if ft.is_dir() {
                if !skip(&name) {
                    stack.push(path);
                }
            } else if ft.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if let Some(lang) = cortex::ext_lang(ext) {
                        *counts.entry(lang).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    let mut v: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(k, n)| (k.to_string(), n))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

pub async fn handle_top_cortex(args: TopCortexArgs) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow!("run_postgres_migrations: {e}"))?;

    match args.command {
        TopCortexCommand::Index {
            path,
            slug,
            lang,
            incremental,
        } => {
            let (root, slug) = resolve_root_slug(path, slug)?;
            run_index(&pool, &root, &slug, lang, true, incremental).await?;
        }
        TopCortexCommand::Embed {
            max,
            corpus,
            no_community,
        } => {
            run_embed(&pool, max, corpus, no_community).await?;
        }
        TopCortexCommand::Summarize {
            all,
            max,
            min_members,
            llm,
            model,
            format,
        } => {
            run_summarize(&pool, all, max, min_members, llm, model, &format).await?;
        }
        TopCortexCommand::Hook { action } => {
            handle_hook(action)?;
        }
        TopCortexCommand::Watch {
            path,
            slug,
            lang,
            debounce,
        } => {
            let (root, slug) = resolve_root_slug(path, slug)?;
            run_watch(&pool, &root, &slug, lang, debounce).await?;
        }
        TopCortexCommand::Status { all, slug, format } => {
            let rows = corpus::list_corpora(&pool).await?;
            let target = if all {
                None
            } else {
                Some(slug.unwrap_or_else(cwd_slug))
            };
            let filtered: Vec<_> = rows
                .into_iter()
                .filter(|r| target.as_deref().map(|t| t == r.slug).unwrap_or(true))
                .collect();

            // list_corpora's `content` count only covers `content:%` nodes and
            // excludes every `code:%` symbol Cortex inserts, so query those
            // separately per corpus for an accurate code-symbol total.
            let mut code_symbols: Vec<i64> = Vec::with_capacity(filtered.len());
            for r in &filtered {
                code_symbols.push(ff_db::pg_count_corpus_code_symbols(&pool, &r.slug).await?);
            }

            if format == "json" {
                let v: Vec<_> = filtered
                    .iter()
                    .zip(code_symbols.iter())
                    .map(|(r, code)| {
                        serde_json::json!({
                            "slug": r.slug, "title": r.title, "sources": r.sources,
                            "entities": r.entities, "facets": r.facets, "content": r.content,
                            "code_symbols": code,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else if filtered.is_empty() {
                if let Some(t) = target {
                    println!(
                        "no corpus '{t}' for this directory yet \u{2014} run `ff cortex index`"
                    );
                } else {
                    println!("no corpora indexed yet \u{2014} run `ff cortex index`");
                }
            } else {
                println!(
                    "{:<22} {:<22} {:>7} {:>8} {:>9} {:>8}",
                    "SLUG", "TITLE", "SOURCES", "CONTENT", "CODE-SYMS", "FACETS"
                );
                for (r, code) in filtered.iter().zip(code_symbols.iter()) {
                    println!(
                        "{:<22} {:<22} {:>7} {:>8} {:>9} {:>8}",
                        r.slug, r.title, r.sources, r.content, code, r.facets
                    );
                }
            }
        }
        TopCortexCommand::Callers {
            symbol,
            corpus,
            format,
        } => {
            let corpus = corpus.unwrap_or_else(cwd_slug);
            crate::cortex_cmd::handle_cortex(
                &pool,
                crate::CortexCommand::Callers {
                    corpus,
                    symbol,
                    format,
                },
            )
            .await?;
        }
        TopCortexCommand::Callees {
            symbol,
            corpus,
            format,
        } => {
            let corpus = corpus.unwrap_or_else(cwd_slug);
            crate::cortex_cmd::handle_cortex(
                &pool,
                crate::CortexCommand::Callees {
                    corpus,
                    symbol,
                    format,
                },
            )
            .await?;
        }
        TopCortexCommand::Find {
            query,
            corpus,
            semantic,
            limit,
            format,
        } => {
            let corpus = corpus.unwrap_or_else(cwd_slug);
            let hits = if semantic {
                cortex::find_symbols_semantic(&pool, &corpus, &query, limit).await?
            } else {
                cortex::find_symbols(&pool, &corpus, &query, limit).await?
            };
            print_hits(&hits, &format, &query, &corpus);
        }
        TopCortexCommand::Impact {
            symbol,
            corpus,
            max_depth,
            format,
        } => {
            let corpus = corpus.unwrap_or_else(cwd_slug);
            crate::cortex_cmd::handle_cortex(
                &pool,
                crate::CortexCommand::Impact {
                    corpus,
                    symbol,
                    max_depth,
                    format,
                },
            )
            .await?;
        }
        TopCortexCommand::Review {
            base,
            path,
            corpus,
            depth,
            format,
        } => {
            let (root, slug) = resolve_root_slug(path, corpus)?;
            run_review(&pool, &root, &slug, base.as_deref(), depth, &format).await?;
        }
    }
    Ok(())
}

/// Render `ff cortex find` hits. `table` (default) shows kind/fan-in/location/
/// symbol; `json` emits the full records (for agents/tooling); `names` lists
/// just the qualified names (pipe straight into callers/callees/impact).
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
                "{CYAN}\u{25b6} cortex find '{query}' in '{corpus}' \u{2014} {} hit(s):{RESET}",
                hits.len()
            );
            if hits.is_empty() {
                println!("  (none \u{2014} try a shorter fragment, or `ff cortex index` if stale)");
                return;
            }
            for h in hits {
                // Strip the `code:` prefix for a compact kind tag (function/class/...).
                let kind = h.node_type.strip_prefix("code:").unwrap_or(&h.node_type);
                let loc = match (&h.file, h.start_line) {
                    (Some(f), Some(l)) => format!("{f}:{l}"),
                    (Some(f), None) => f.clone(),
                    _ => "-".to_string(),
                };
                // Semantic hits carry a similarity score (the ranking key); show it
                // so the order is legible. Substring hits rank by fan-in alone.
                match h.score {
                    Some(s) => println!(
                        "  {:<9} sim={:.2} fanin={:<4} {}  ({loc})",
                        kind, s, h.fan_in, h.qualified_name
                    ),
                    None => println!(
                        "  {:<9} fanin={:<4} {}  ({loc})",
                        kind, h.fan_in, h.qualified_name
                    ),
                }
            }
        }
    }
}

/// `ff cortex review`: derive changed files from `git diff`, score them against
/// the Cortex graph, and print a risk-ranked review map.
async fn run_review(
    pool: &PgPool,
    root: &Path,
    slug: &str,
    base: Option<&str>,
    depth: usize,
    format: &str,
) -> Result<()> {
    let changed_rel = git_changed_files(root, base)?;
    // Keep only Cortex-supported source files (skip docs/config/etc) and map
    // repo-relative → absolute (the path form stored on content:file nodes).
    let changed_abs: Vec<String> = changed_rel
        .iter()
        .filter(|rel| {
            Path::new(rel)
                .extension()
                .and_then(|e| e.to_str())
                .and_then(cortex::ext_lang)
                .map(|l| cortex::SUPPORTED_LANGS.contains(&l))
                .unwrap_or(false)
        })
        .map(|rel| root.join(rel).to_string_lossy().to_string())
        .collect();

    if changed_abs.is_empty() {
        if format == "json" {
            println!(
                "{}",
                serde_json::to_string_pretty(&cortex::ReviewReport::default())?
            );
        } else {
            let scope = base.map(|b| format!(" vs {b}")).unwrap_or_default();
            println!("no changed Cortex-supported source files{scope} — nothing to review");
        }
        return Ok(());
    }

    // Hunk-level refinement: which line ranges the diff actually touched, in
    // working-tree coordinates (the same file revision Cortex parsed). Keyed by
    // absolute path to match `changed_abs`. Best-effort — if the diff can't be
    // read or parsed, review falls back to file-level granularity.
    let changed_lines = git_changed_line_ranges(root, base).unwrap_or_default();

    let report = cortex::review(pool, slug, &changed_abs, depth, Some(&changed_lines)).await?;

    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let root_str = root.to_string_lossy().to_string();
    let rel = |p: &str| -> String {
        p.strip_prefix(&root_str)
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| p.to_string())
    };

    let scope = base.map(|b| format!(" (vs {b})")).unwrap_or_default();
    let gran = if report.hunk_level {
        " [hunk-level]"
    } else {
        ""
    };
    println!(
        "{CYAN}\u{25b6} cortex review: corpus '{}'{scope} \u{2014} {} changed file(s), blast radius {} symbol(s){gran}{RESET}",
        slug,
        report.files.len(),
        report.total_blast
    );
    if report.files.is_empty() && report.unindexed.is_empty() {
        println!("  (no indexed symbols touched)");
    }
    for f in &report.files {
        let color = match f.risk {
            cortex::RiskTier::High => YELLOW,
            _ => RESET,
        };
        println!(
            "\n{color}{:<4}{RESET} {}  (blast {})",
            f.risk.label(),
            rel(&f.path),
            f.blast_radius
        );
        // Surface the callable, fan-in-bearing symbols (skip zero-impact
        // type/impl nodes); cap the list so a hub-heavy file's highest-risk
        // changes stay visible instead of scrolling off. `--format json` keeps
        // every symbol for tooling.
        let fns: Vec<&cortex::ChangedSymbol> = f
            .symbols
            .iter()
            .filter(|s| s.node_type == "code:function")
            .collect();
        const MAX_SYMS: usize = 12;
        for s in fns.iter().take(MAX_SYMS) {
            let callers = if s.top_callers.is_empty() {
                String::new()
            } else {
                format!("  e.g. {}", s.top_callers.join(", "))
            };
            println!(
                "  {:<4} {}  fanin={} (ext={})  blast={}{}",
                s.risk.label(),
                s.qualified_name,
                s.direct_callers,
                s.external_callers,
                s.blast_radius,
                callers
            );
        }
        if fns.len() > MAX_SYMS {
            println!(
                "  \u{2026} +{} more changed function(s) (use --format json for all)",
                fns.len() - MAX_SYMS
            );
        }
    }
    if !report.unindexed.is_empty() {
        println!(
            "\n{YELLOW}unindexed (not in graph yet){RESET}: {}",
            report
                .unindexed
                .iter()
                .map(|p| rel(p))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("  run `ff cortex index` to cover new files.");
    }
    Ok(())
}

/// Changed files (repo-relative) from `git diff`. With `base`, reviews the
/// branch's own commits (`base...HEAD`) plus uncommitted edits; without it,
/// just uncommitted work (staged + unstaged + untracked) vs HEAD. Deduped.
fn git_changed_files(root: &Path, base: Option<&str>) -> Result<Vec<String>> {
    use std::collections::BTreeSet;
    use std::process::Command;

    let run = |args: &[&str]| -> Result<Vec<String>> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .map_err(|e| anyhow!("run git {}: {e}", args.join(" ")))?;
        if !out.status.success() {
            return Err(anyhow!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    };

    let mut files: BTreeSet<String> = BTreeSet::new();
    if let Some(b) = base {
        // Branch's own changes since it diverged from base.
        for f in run(&["diff", "--name-only", &format!("{b}...HEAD")])? {
            files.insert(f);
        }
    }
    // Uncommitted tracked changes (staged + unstaged) vs HEAD.
    for f in run(&["diff", "--name-only", "HEAD"])? {
        files.insert(f);
    }
    // Untracked-but-not-ignored files (new source the diff above misses).
    for f in run(&["ls-files", "--others", "--exclude-standard"])? {
        files.insert(f);
    }
    Ok(files.into_iter().collect())
}

/// Changed line ranges per file (absolute path → 1-based inclusive `(start,end)`
/// ranges in the WORKING-TREE revision — the same file Cortex parsed). Uses a
/// single two-dot diff so every range is in one coordinate space: `git diff
/// <base>` (working tree vs base) when reviewing a branch, else `git diff HEAD`
/// (uncommitted vs HEAD). New/untracked files are absent here on purpose → review
/// falls back to file-level for them (everything in a new file is new anyway).
fn git_changed_line_ranges(
    root: &Path,
    base: Option<&str>,
) -> Result<std::collections::HashMap<String, Vec<(u32, u32)>>> {
    use std::process::Command;
    let mut args = vec!["diff", "--unified=0", "--no-color"];
    let base_spec;
    if let Some(b) = base {
        base_spec = b.to_string();
        args.push(&base_spec);
    } else {
        args.push("HEAD");
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(&args)
        .output()
        .map_err(|e| anyhow!("run git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let diff = String::from_utf8_lossy(&out.stdout);
    let by_rel = cortex::parse_diff_line_ranges(&diff);
    // Map repo-relative → absolute (the key form review() matches on).
    Ok(by_rel
        .into_iter()
        .map(|(rel, ranges)| (root.join(rel).to_string_lossy().to_string(), ranges))
        .collect())
}

/// Derive the corpus slug for the current working directory (same rule as
/// `ff cortex index` uses), so callers/callees/impact/status need no slug arg.
fn cwd_slug() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    slug_from_path(&cwd)
}

/// Canonicalize PATH (default cwd) and derive the corpus slug from it.
fn resolve_root_slug(path: Option<String>, slug: Option<String>) -> Result<(PathBuf, String)> {
    let raw = path.unwrap_or_else(|| ".".to_string());
    let root = std::fs::canonicalize(&raw).map_err(|e| anyhow!("resolve path '{raw}': {e}"))?;
    let slug = slug.unwrap_or_else(|| slug_from_path(&root));
    Ok((root, slug))
}

/// Core index routine shared by `ff cortex index` and `ff cortex watch`.
/// Create-or-reuse the corpus, auto-detect language(s), index each.
/// `verbose` controls the human-readable banner output.
async fn run_index(
    pool: &PgPool,
    root: &Path,
    slug: &str,
    lang: Option<String>,
    verbose: bool,
    incremental: bool,
) -> Result<()> {
    let root_str = root.to_string_lossy().to_string();

    // Decide which language(s) to index.
    let langs: Vec<String> = if let Some(l) = lang {
        vec![l]
    } else {
        let detected = detect_languages(root);
        if detected.is_empty() {
            return Err(anyhow!(
                "no rust/typescript/javascript/java/python source files found under {root_str}; \
                 pass --lang to force one"
            ));
        }
        // Pick the dominant language, plus any other that has at least 25% of
        // the dominant's file count OR at least 50 files outright. The relative
        // rule kills stray-file noise (2 .js helpers in a Rust repo); the
        // absolute floor keeps substantial secondary languages that a huge
        // dominant one would otherwise mask (HireFlow360: 925 Java files were
        // silently dropped because 5500+ TS/TSX files made the 25% bar 1385).
        let top = detected[0].1.max(1);
        detected
            .into_iter()
            .filter(|(_, n)| *n * 4 >= top || *n >= 50)
            .map(|(l, _)| l)
            .collect()
    };
    // Cortex parses a subset of what detection can see — index what's
    // supported, note what isn't (e.g. python, for now).
    let (langs, skipped): (Vec<String>, Vec<String>) = langs
        .into_iter()
        .partition(|l| cortex::SUPPORTED_LANGS.contains(&l.as_str()));
    if !skipped.is_empty() {
        println!(
            "{YELLOW}  skipping unsupported language(s): {}{RESET}",
            skipped.join(", ")
        );
    }
    if langs.is_empty() {
        return Err(anyhow!(
            "no Cortex-supported source files ({}) under {root_str}; pass --lang to force one",
            cortex::SUPPORTED_LANGS.join("/")
        ));
    }

    // Create-or-reuse the corpus (add_corpus does ON CONFLICT DO UPDATE,
    // identical to `ff brain corpus add <slug> --root <root>`).
    let c = corpus::add_corpus(
        pool,
        slug,
        slug,
        &[(root_str.clone(), Some("code".to_string()))],
    )
    .await?;
    // Walk the source roots NOW: Cortex reads only what the corpus scan
    // recorded as content:file nodes, so a fresh (or stale) corpus must be
    // (re)scanned or indexing sees zero files.
    let scan_report = corpus::scan(pool, &c, None, 12).await?;
    if verbose {
        println!(
            "{CYAN}\u{25b6} cortex: corpus '{}' \u{2190} {}{RESET}",
            c.slug, root_str
        );
        println!("  language(s): {}", langs.join(", "));
        println!(
            "  scanned: {} file(s), {} dir(s)",
            scan_report.files, scan_report.dirs
        );
        if scan_report.pruned > 0 {
            println!(
                "  pruned: {} stale out-of-root content node(s)",
                scan_report.pruned
            );
        }
    }

    let mut total_symbols = 0usize;
    let mut total_edges = 0usize;
    let mut total_files = 0usize;
    // Full: index_langs wipes prior code:* nodes ONCE, then extracts each
    // language (per-language cortex::index calls would clobber each other).
    // Incremental: re-extract only files whose content_hash changed since the
    // last index (and delete symbols of removed files) — no global rewipe.
    let per_lang = if incremental {
        let report = cortex::index_langs_incremental(pool, slug, &langs).await?;
        if verbose {
            println!(
                "{CYAN}  incremental: {} changed, {} unchanged, {} deleted{RESET}",
                report.files_changed, report.files_unchanged, report.files_deleted
            );
        }
        report.per_lang
    } else {
        cortex::index_langs(pool, slug, &langs).await?
    };
    for (l, stats) in &per_lang {
        if verbose {
            println!(
                "  {l:<11} files={} symbols={} contains={} imports={} calls={}/{}",
                stats.files_parsed,
                stats.symbols,
                stats.contains,
                stats.imports,
                stats.calls_resolved,
                stats.calls_total
            );
        }
        total_files += stats.files_parsed;
        total_symbols += stats.symbols;
        total_edges += stats.contains + stats.imports + stats.calls_resolved;
    }

    // STEP 1 of multi-domain Cortex: also index DOCUMENTS (.md/.txt/...) for this
    // root. Best-effort — a doc-index error must never fail the whole index.
    match ff_brain::doc_index::index_docs(pool, slug, root).await {
        Ok(doc_stats) => {
            total_files += doc_stats.files;
            total_symbols += doc_stats.sections;
            total_edges += doc_stats.edges;
            if verbose {
                println!(
                    "  {:<11} files={} sections={}",
                    "docs", doc_stats.files, doc_stats.sections
                );
            } else {
                println!(
                    "  docs: {} files, {} sections",
                    doc_stats.files, doc_stats.sections
                );
            }
        }
        Err(e) => {
            eprintln!("  docs: skipped ({e})");
        }
    }

    // STEP 2 of multi-domain Cortex: also index structured/financial DATA
    // (.csv/.tsv) for this root. Best-effort — never fails the whole index.
    match ff_brain::data_index::index_data(pool, slug, root).await {
        Ok(data_stats) => {
            total_files += data_stats.files;
            total_symbols += data_stats.columns;
            total_edges += data_stats.edges;
            if verbose {
                println!(
                    "  {:<11} files={} columns={} rows={}",
                    "data", data_stats.files, data_stats.columns, data_stats.rows
                );
            } else {
                println!(
                    "  data: {} files, {} columns",
                    data_stats.files, data_stats.columns
                );
            }
        }
        Err(e) => {
            eprintln!("  data: skipped ({e})");
        }
    }

    // STEP 3 of multi-domain Cortex: also index IMAGES (.png/.jpg/...) for this
    // root, with a bounded best-effort vision caption/tag pass. Best-effort —
    // an image-index error must never fail the whole index.
    match ff_brain::image_index::index_images(pool, slug, root).await {
        Ok(image_stats) => {
            total_files += image_stats.files;
            total_symbols += image_stats.tags;
            total_edges += image_stats.edges;
            if verbose {
                println!(
                    "  {:<11} files={} captioned={} tags={}",
                    "images", image_stats.files, image_stats.captioned, image_stats.tags
                );
            } else {
                println!(
                    "  images: {} files, {} captioned",
                    image_stats.files, image_stats.captioned
                );
            }
        }
        Err(e) => {
            eprintln!("  images: skipped ({e})");
        }
    }

    if verbose {
        println!(
            "{CYAN}\u{2713} corpus '{}' indexed: {} file(s), {} node(s), {} edge(s){RESET}",
            slug, total_files, total_symbols, total_edges
        );
        println!("  try: ff cortex callers <symbol>   |   ff cortex status");
    } else {
        println!(
            "{CYAN}\u{2713} re-indexed '{}': {} file(s), {} node(s), {} edge(s){RESET}",
            slug, total_files, total_symbols, total_edges
        );
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// semantic embedding + community detection
// ----------------------------------------------------------------------------

/// `ff cortex embed`: fill the `embedding` column on every Cortex node via the
/// fleet's bge-m3 endpoint, then (unless suppressed) run community detection.
async fn run_embed(
    pool: &sqlx::PgPool,
    max: Option<usize>,
    corpus: Option<String>,
    no_community: bool,
) -> Result<()> {
    match &corpus {
        Some(c) => println!("{CYAN}▶ Embedding Cortex nodes for corpus '{c}'...{RESET}"),
        None => println!("{CYAN}▶ Embedding Cortex nodes (code/doc/data/image)...{RESET}"),
    }
    let stats =
        ff_brain::embed_cortex_nodes(pool, max, corpus.as_deref(), |embedded, remaining| {
            // Live counter overwritten in place; remaining < 0 means the count
            // query failed (non-fatal) — show a dash rather than a bogus number.
            let rem = if remaining < 0 {
                "?".to_string()
            } else {
                remaining.to_string()
            };
            print!("\r  embedded {embedded}  ·  remaining {rem}   ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        })
        .await
        .map_err(|e| anyhow!("embed Cortex nodes: {e}"))?;

    println!(
        "\r{GREEN}✓{RESET} embedded {} node(s){}; {} still unembedded   ",
        stats.embedded,
        if stats.failed > 0 {
            format!(" ({} failed)", stats.failed)
        } else {
            String::new()
        },
        stats.remaining.max(0),
    );

    if no_community {
        return Ok(());
    }
    // Community detection is a fleet-wide graph pass, not corpus-scoped — running
    // it after a single-corpus embed would do global work the caller didn't ask
    // for (and the graph is only partly embedded). Skip it; the fleet-wide
    // `ff cortex embed` (or `ff cortex summarize`) refreshes communities.
    if corpus.is_some() {
        println!(
            "{YELLOW}⏭  skipping community detection (corpus-scoped embed) — \
             run `ff cortex embed` fleet-wide to refresh communities{RESET}"
        );
        return Ok(());
    }

    println!("{CYAN}▶ Detecting communities over the graph...{RESET}");
    match ff_brain::detect_communities(pool).await {
        Ok(summary) => println!(
            "{GREEN}✓{RESET} {} communities (largest: {} nodes), {} persisted to registry",
            summary.communities_found, summary.largest_community, summary.communities_persisted
        ),
        Err(e) => println!("{YELLOW}⚠ community detection failed: {e}{RESET}"),
    }
    Ok(())
}

/// `ff cortex summarize`: for each detected community, ask a fleet LLM what the
/// cluster is responsible for and store the summary on `brain_communities`.
#[allow(clippy::too_many_arguments)]
async fn run_summarize(
    pool: &PgPool,
    all: bool,
    max: usize,
    min_members: usize,
    llm: Option<String>,
    model: Option<String>,
    format: &str,
) -> Result<()> {
    let opts = ff_brain::SummarizeOpts {
        all,
        max,
        min_members,
        endpoint: llm,
        model,
    };

    let human = format != "json";
    if human {
        println!(
            "{CYAN}▶ Summarizing communities ({}, min_members={min_members}, max={max})...{RESET}",
            if all {
                "all eligible"
            } else {
                "un-summarized only"
            }
        );
    }

    let stats = ff_brain::summarize_communities(pool, &opts, |done, total| {
        if human {
            print!("\r  summarized {done}/{total}   ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    })
    .await
    .map_err(|e| anyhow!("summarize communities: {e}"))?;

    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&stats)?);
        return Ok(());
    }

    if stats.eligible == 0 {
        println!(
            "\r{GREEN}✓{RESET} no communities need a summary (run `ff cortex embed` first to detect communities, or pass --all to refresh)   "
        );
        return Ok(());
    }

    println!(
        "\r{GREEN}✓{RESET} {} summarized via {} ({}){}{}   ",
        stats.summarized,
        stats.endpoint,
        stats.model,
        if stats.failed > 0 {
            format!(", {} failed", stats.failed)
        } else {
            String::new()
        },
        if stats.empty > 0 {
            format!(", {} empty", stats.empty)
        } else {
            String::new()
        },
    );
    if stats.eligible > stats.attempted {
        println!(
            "  {} more eligible — re-run to continue (or raise --max).",
            stats.eligible - stats.attempted
        );
    }
    if !stats.samples.is_empty() {
        println!("\n  samples (inspect quality):");
        for s in &stats.samples {
            println!(
                "  {YELLOW}#{}{RESET} {} ({} members)\n    {}",
                s.community_id, s.god_title, s.member_count, s.summary
            );
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// git post-commit hook (mirrors `graphify hook install/uninstall`)
// ----------------------------------------------------------------------------

fn handle_hook(action: HookAction) -> Result<()> {
    match action {
        HookAction::Install { path } => {
            let (root, _slug) = resolve_root_slug(path, None)?;
            hook_install(&root)
        }
        HookAction::Uninstall { path } => {
            let (root, _slug) = resolve_root_slug(path, None)?;
            hook_uninstall(&root)
        }
    }
}

/// Resolve `<repo>/.git/hooks/post-commit`, verifying `<repo>/.git` exists.
fn hook_path(root: &Path) -> Result<PathBuf> {
    let git_dir = root.join(".git");
    if !git_dir.exists() {
        return Err(anyhow!(
            "{} is not a git repository (no .git directory)",
            root.display()
        ));
    }
    Ok(git_dir.join("hooks").join("post-commit"))
}

fn hook_install(root: &Path) -> Result<()> {
    let hook = hook_path(root)?;
    if let Some(parent) = hook.parent() {
        std::fs::create_dir_all(parent).map_err(|e| anyhow!("create {}: {e}", parent.display()))?;
    }

    let existing = std::fs::read_to_string(&hook).unwrap_or_default();
    if existing.contains(HOOK_BEGIN) {
        println!(
            "{CYAN}\u{2713} ff cortex post-commit hook already installed at {}{RESET}",
            hook.display()
        );
        return Ok(());
    }

    let root_str = root.to_string_lossy();
    // The block re-indexes against the absolute repo path so it works regardless
    // of the cwd the commit was made from. `ff cortex index` is create-or-reuse +
    // re-extract. `--incremental` re-extracts only the files a commit changed
    // (cheap: the common case is a handful of files vs a full graph rewipe).
    let block = format!(
        "{HOOK_BEGIN}\n# Auto-installed by `ff cortex hook install` — re-indexes the Cortex graph after each commit.\nff cortex index --incremental \"{root_str}\" >/dev/null 2>&1 || true\n{HOOK_END}\n"
    );

    let new_contents = if existing.trim().is_empty() {
        format!("#!/bin/sh\n{block}")
    } else if existing.ends_with('\n') {
        format!("{existing}{block}")
    } else {
        format!("{existing}\n{block}")
    };

    std::fs::write(&hook, new_contents).map_err(|e| anyhow!("write {}: {e}", hook.display()))?;
    make_executable(&hook)?;

    println!(
        "{CYAN}\u{2713} installed ff cortex post-commit hook at {}{RESET}",
        hook.display()
    );
    println!("  every `git commit` will now re-index this repo's Cortex graph.");
    Ok(())
}

fn hook_uninstall(root: &Path) -> Result<()> {
    let hook = hook_path(root)?;
    let existing = match std::fs::read_to_string(&hook) {
        Ok(s) => s,
        Err(_) => {
            println!(
                "no post-commit hook at {} \u{2014} nothing to do",
                hook.display()
            );
            return Ok(());
        }
    };
    if !existing.contains(HOOK_BEGIN) {
        println!(
            "post-commit hook at {} has no ff cortex block \u{2014} left untouched",
            hook.display()
        );
        return Ok(());
    }

    // Strip the managed block (and a single trailing blank line it may leave).
    let mut out = String::with_capacity(existing.len());
    let mut in_block = false;
    for line in existing.lines() {
        if line.trim() == HOOK_BEGIN {
            in_block = true;
            continue;
        }
        if line.trim() == HOOK_END {
            in_block = false;
            continue;
        }
        if !in_block {
            out.push_str(line);
            out.push('\n');
        }
    }

    // If nothing meaningful remains (just a shebang or whitespace), remove the file.
    let meaningful = out
        .lines()
        .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with("#!"));
    if meaningful {
        std::fs::write(&hook, out).map_err(|e| anyhow!("write {}: {e}", hook.display()))?;
        println!(
            "{CYAN}\u{2713} removed ff cortex block from {}{RESET}",
            hook.display()
        );
    } else {
        std::fs::remove_file(&hook).map_err(|e| anyhow!("remove {}: {e}", hook.display()))?;
        println!(
            "{CYAN}\u{2713} removed ff cortex post-commit hook ({} had no other content){RESET}",
            hook.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| anyhow!("stat {}: {e}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| anyhow!("chmod {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

// ----------------------------------------------------------------------------
// foreground watcher (notify crate — already vendored in the workspace)
// ----------------------------------------------------------------------------

async fn run_watch(
    pool: &PgPool,
    root: &Path,
    slug: &str,
    lang: Option<String>,
    debounce_secs: u64,
) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc::channel;
    use std::time::Duration;

    // Index once up front so the graph is current before we start watching.
    println!(
        "{CYAN}\u{25b6} cortex watch: indexing '{}' once, then watching {}\u{2026}{RESET}",
        slug,
        root.display()
    );
    // Full index once up front (also populates the incremental ledger), then
    // every on-change re-index below is incremental.
    run_index(pool, root, slug, lang.clone(), true, false).await?;

    let (tx, rx) = channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // Only care about content/structure changes to source-ish files.
            if !matches!(
                event.kind,
                notify::EventKind::Create(_)
                    | notify::EventKind::Modify(_)
                    | notify::EventKind::Remove(_)
            ) {
                return;
            }
            let relevant = event.paths.iter().any(|p| is_watchable(p));
            if relevant {
                let _ = tx.send(());
            }
        }
    })
    .map_err(|e| anyhow!("create file watcher: {e}"))?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|e| anyhow!("watch {}: {e}", root.display()))?;

    println!(
        "  watching for changes (debounce {}s) \u{2014} Ctrl-C to stop.",
        debounce_secs
    );

    let debounce = Duration::from_secs(debounce_secs.max(1));
    loop {
        // Block until the first change event arrives.
        if rx.recv().is_err() {
            break; // sender dropped — watcher gone
        }
        // Debounce: drain any further events that land within the window so a
        // burst of edits collapses into a single re-index.
        loop {
            match rx.recv_timeout(debounce) {
                Ok(()) => continue, // more changes — keep draining
                Err(_) => break,    // quiet for `debounce` — go index
            }
        }
        println!(
            "{CYAN}\u{25b6} change detected \u{2014} re-indexing (incremental)\u{2026}{RESET}"
        );
        if let Err(e) = run_index(pool, root, slug, lang.clone(), false, true).await {
            eprintln!("  re-index failed: {e}");
        }
    }
    Ok(())
}

/// Should a changed path trigger a re-index? Source files only, skipping the
/// heavy/vendored dirs (and `.git`, so commits don't self-trigger forever).
fn is_watchable(p: &Path) -> bool {
    for comp in p.components() {
        if let std::path::Component::Normal(os) = comp {
            let name = os.to_string_lossy();
            if matches!(
                name.as_ref(),
                ".git"
                    | "target"
                    | "node_modules"
                    | "dist"
                    | "build"
                    | ".venv"
                    | "venv"
                    | "__pycache__"
                    | ".next"
                    | "vendor"
            ) {
                return false;
            }
        }
    }
    p.extension()
        .and_then(|e| e.to_str())
        .map(|ext| cortex::ext_lang(ext).is_some())
        .unwrap_or(false)
}
