//! Top-level `ff cortex …` — a graphify-grade ergonomic wrapper around the
//! `ff brain corpus add` + `ff brain cortex index` two-step. It reuses the exact
//! same graph logic (ff_brain::corpus + ff_brain::cortex) and the existing
//! brain cortex handler, so behavior is identical to the long form.

use crate::{CYAN, RESET};
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
        /// Force a specific language instead of auto-detecting (rust/typescript/javascript/python).
        #[arg(long)]
        lang: Option<String>,
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

/// Map a file extension to a Cortex language name (None = ignored).
fn ext_lang(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "py" => Some("python"),
        _ => None,
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
                    if let Some(lang) = ext_lang(ext) {
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
        TopCortexCommand::Index { path, slug, lang } => {
            let (root, slug) = resolve_root_slug(path, slug)?;
            run_index(&pool, &root, &slug, lang, true).await?;
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

            if format == "json" {
                let v: Vec<_> = filtered
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "slug": r.slug, "title": r.title, "sources": r.sources,
                            "entities": r.entities, "facets": r.facets, "content": r.content,
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
                    "{:<22} {:<22} {:>7} {:>8} {:>8}",
                    "SLUG", "TITLE", "SOURCES", "NODES", "FACETS"
                );
                for r in &filtered {
                    println!(
                        "{:<22} {:<22} {:>7} {:>8} {:>8}",
                        r.slug, r.title, r.sources, r.content, r.facets
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
    }
    Ok(())
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
) -> Result<()> {
    let root_str = root.to_string_lossy().to_string();

    // Decide which language(s) to index.
    let langs: Vec<String> = if let Some(l) = lang {
        vec![l]
    } else {
        let detected = detect_languages(root);
        if detected.is_empty() {
            return Err(anyhow!(
                "no rust/typescript/javascript/python source files found under {root_str}; \
                 pass --lang to force one"
            ));
        }
        // Pick the dominant language, plus any other that has at least
        // 25% of the dominant's file count (so polyglot repos get both).
        let top = detected[0].1.max(1);
        detected
            .into_iter()
            .filter(|(_, n)| *n * 4 >= top)
            .map(|(l, _)| l)
            .collect()
    };

    // Create-or-reuse the corpus (add_corpus does ON CONFLICT DO UPDATE,
    // identical to `ff brain corpus add <slug> --root <root>`).
    let c = corpus::add_corpus(
        pool,
        slug,
        slug,
        &[(root_str.clone(), Some("code".to_string()))],
    )
    .await?;
    if verbose {
        println!(
            "{CYAN}\u{25b6} cortex: corpus '{}' \u{2190} {}{RESET}",
            c.slug, root_str
        );
        println!("  language(s): {}", langs.join(", "));
    }

    let mut total_symbols = 0usize;
    let mut total_edges = 0usize;
    let mut total_files = 0usize;
    for l in &langs {
        if verbose {
            println!("{CYAN}\u{25b6} indexing {l}\u{2026}{RESET}");
        }
        let stats = cortex::index(pool, slug, l).await?;
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
    // re-extract, i.e. incremental at the corpus level.
    let block = format!(
        "{HOOK_BEGIN}\n# Auto-installed by `ff cortex hook install` — re-indexes the Cortex graph after each commit.\nff cortex index \"{root_str}\" >/dev/null 2>&1 || true\n{HOOK_END}\n"
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
    run_index(pool, root, slug, lang.clone(), true).await?;

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
        println!("{CYAN}\u{25b6} change detected \u{2014} re-indexing\u{2026}{RESET}");
        if let Err(e) = run_index(pool, root, slug, lang.clone(), false).await {
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
        .map(|ext| ext_lang(ext).is_some())
        .unwrap_or(false)
}
