//! Leader-gated cortex REINDEX loop — keeps the self-corpus code graph fresh.
//!
//! forgefleetd already runs an embed-refresh tick (drains NULL embeddings) and a
//! community-summary tick, but NEITHER re-parses changed source into the graph —
//! they only maintain metadata over already-indexed nodes. So once nobody runs
//! `ff cortex index` by hand, the graph structure silently goes stale: new/changed
//! files never enter it, and `cortex_find` misses just-written symbols (observed
//! 2026-06-19 — the forge-fleet corpus was 4 days behind HEAD and `cortex_find
//! fleet_oneshot` returned 0 hits). This tick closes that gap: re-scan the self
//! corpus + incrementally re-index it (hash-diffed, so unchanged files are
//! skipped — cheap). The embed tick then picks up the freshly-indexed nodes.
//!
//! Leader-gated (only the leader's checkout drives the canonical corpora), every
//! 10 min, runs by DEFAULT; opt out with `fleet_secrets.cortex_index_mode=off`.
//! Covers the comma-separated corpora in `fleet_secrets.cortex_reindex_corpora`
//! (default just `forge-fleet`), each re-indexed with its OWN auto-detected
//! languages — so a polyglot repo (hireflow360: java/ts/python) stays fresh too,
//! not only Rust. A corpus whose checkout isn't on the leader is a quiet skip.

use anyhow::Result;
use sqlx::PgPool;

use crate::{corpus, cortex};

/// The corpus kept fresh when the operator hasn't configured a list — ForgeFleet's
/// own source tree (the graph agents query about ff itself).
const DEFAULT_CORPORA: &str = "forge-fleet";
/// fleet_secrets key holding the comma-separated corpus slugs to keep fresh.
/// Each is re-scanned + incrementally re-indexed every tick with its OWN
/// auto-detected languages — so a polyglot repo (hireflow360: java/ts/python) is
/// kept current too, not just Rust. Set via `ff secrets set cortex_reindex_corpora`.
const CORPORA_KEY: &str = "cortex_reindex_corpora";
const INDEX_MODE_KEY: &str = "cortex_index_mode";

/// `off`/`false`/`0`/`disabled`/`no` (case-insensitive) disable the tick; any
/// other value — including a missing secret — leaves it ON.
fn mode_is_off(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_lowercase()).as_deref(),
        Some("off" | "false" | "0" | "disabled" | "no")
    )
}

async fn reindex_disabled(pool: &PgPool) -> bool {
    match ff_db::pg_get_secret(pool, INDEX_MODE_KEY).await {
        Ok(v) => mode_is_off(v.as_deref()),
        Err(e) => {
            tracing::warn!(error = %e, "cortex reindex: failed to read mode secret; defaulting on");
            false
        }
    }
}

/// The corpus slugs to keep fresh this tick, from `fleet_secrets.CORPORA_KEY`
/// (comma-separated), falling back to the self corpus.
async fn configured_corpora(pool: &PgPool) -> Vec<String> {
    let raw = ff_db::pg_get_secret(pool, CORPORA_KEY)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| DEFAULT_CORPORA.to_string());
    parse_corpora_list(&raw)
}

/// Pure split/trim/dedup of a comma-separated corpus list; empty input yields the
/// default self corpus so the tick is never a silent no-op on a blank secret.
fn parse_corpora_list(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in raw.split(',') {
        let s = s.trim();
        if !s.is_empty() && !out.iter().any(|e| e == s) {
            out.push(s.to_string());
        }
    }
    if out.is_empty() {
        out.push(DEFAULT_CORPORA.to_string());
    }
    out
}

/// Languages to index under `root`: walk the tree counting Cortex-known source
/// files (skipping vendor/build dirs), keep the dominant language plus any
/// secondary with ≥25% of its file count OR ≥50 files, then restrict to
/// Cortex-SUPPORTED languages. Mirrors the `ff cortex index` CLI heuristic so the
/// daemon tick indexes the same set — crucially NOT Rust-only, so a polyglot repo
/// (java/ts/python) is kept fresh.
pub fn detect_index_langs(root: &std::path::Path) -> Vec<String> {
    use std::collections::HashMap;
    fn skip(name: &str) -> bool {
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
    }
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        if visited > 50_000 {
            break;
        }
        visited += 1;
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if ft.is_dir() {
                if !skip(&name) {
                    stack.push(entry.path());
                }
            } else if ft.is_file() {
                if let Some(ext) = entry.path().extension().and_then(|e| e.to_str()) {
                    if let Some(lang) = cortex::ext_lang(ext) {
                        *counts.entry(lang).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    let mut v: Vec<(&'static str, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    let Some(&(_, top)) = v.first() else {
        return Vec::new();
    };
    let top = top.max(1);
    v.into_iter()
        .filter(|(_, n)| *n * 4 >= top || *n >= 50)
        .map(|(l, _)| l.to_string())
        .filter(|l| cortex::SUPPORTED_LANGS.contains(&l.as_str()))
        .collect()
}

/// Re-scan + incrementally re-index ONE corpus by slug. Auto-detects languages
/// across ALL of the corpus's registered source roots that exist on THIS host —
/// a corpus can have several (e.g. hireflow360 = a 50-file docs root at
/// ~/Business/HireFlow360 PLUS the 10k-file code root at ~/projects/HireFlow360);
/// picking one arbitrarily (the old `ORDER BY id LIMIT 1`) grabbed the docs root,
/// found no code, and silently left the corpus stale. Returns `None` when the
/// corpus isn't registered, none of its checkouts are on this host, or none has
/// Cortex-supported source — a quiet skip, not an error. The index pass is
/// retried on a transient Postgres deadlock (it races the hourly embed/summary
/// ticks writing the same graph tables).
pub async fn reindex_corpus(
    pool: &PgPool,
    slug: &str,
) -> Result<Option<cortex::IncrementalReport>> {
    // Every source root for this corpus, code-heaviest first so a small docs root
    // can never shadow the real code root.
    let roots: Vec<String> = sqlx::query_scalar(
        "SELECT bs.root_path FROM brain_sources bs \
           JOIN brain_corpora bc ON bc.id = bs.corpus_id \
          WHERE bc.slug = $1 \
          ORDER BY bs.file_count DESC NULLS LAST, bs.id",
    )
    .bind(slug)
    .fetch_all(pool)
    .await?;

    // Union the detected languages across every root that actually exists here.
    let mut langs: Vec<String> = Vec::new();
    let mut any_root_here = false;
    for root in &roots {
        let p = std::path::Path::new(root);
        if !p.exists() {
            continue; // this source's checkout lives on another host
        }
        any_root_here = true;
        for l in detect_index_langs(p) {
            if !langs.contains(&l) {
                langs.push(l);
            }
        }
    }
    if !any_root_here || langs.is_empty() {
        return Ok(None); // no checkout on this host, or no Cortex-supported code
    }

    // Re-scan the corpus's content:file nodes from disk (the incremental indexer
    // diffs against these), then incrementally re-index. Use get_corpus (NOT
    // add_corpus) so we never clobber a multi-source corpus's registered roots.
    let Some(c) = corpus::get_corpus(pool, slug).await? else {
        return Ok(None);
    };
    corpus::scan(pool, &c, None, 12).await?;

    // Retry the index pass on a transient Postgres deadlock (races embed/summary).
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match cortex::index_langs_incremental(pool, slug, &langs).await {
            Ok(report) => return Ok(Some(report)),
            Err(e) if attempt < 3 && e.to_string().contains("deadlock detected") => {
                tracing::warn!(
                    corpus = slug,
                    attempt,
                    "cortex reindex: deadlock — retrying index pass"
                );
                tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Back-compat wrapper: re-index just the self corpus. Retained for the lib
/// re-export; the tick now iterates [`configured_corpora`].
pub async fn reindex_self_corpus(pool: &PgPool) -> Result<Option<cortex::IncrementalReport>> {
    reindex_corpus(pool, DEFAULT_CORPORA).await
}

/// Spawn the leader-gated cortex reindex loop. Fires on the interval, skips
/// unless this node is the live leader (so only the leader's checkout drives the
/// canonical corpus), honours the `cortex_index_mode` gate, then re-scans +
/// incrementally re-indexes the self corpus. Best-effort — a failed pass logs
/// and resumes next tick; an unregistered corpus is a quiet skip.
pub fn spawn_reindex_loop(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let is_leader: bool = sqlx::query_scalar(
                        r#"
                        SELECT EXISTS (
                            SELECT 1 FROM fleet_leader_state
                            WHERE member_name = $1
                              AND heartbeat_at > NOW() - INTERVAL '60 seconds'
                        )
                        "#,
                    )
                    .bind(&worker_name)
                    .fetch_one(&pg)
                    .await
                    .unwrap_or(false);

                    if !is_leader {
                        continue;
                    }
                    if reindex_disabled(&pg).await {
                        continue;
                    }

                    for slug in configured_corpora(&pg).await {
                        match reindex_corpus(&pg, &slug).await {
                            Ok(Some(r)) if r.files_changed > 0 || r.files_deleted > 0 => {
                                tracing::info!(
                                    corpus = %slug,
                                    files_changed = r.files_changed,
                                    files_deleted = r.files_deleted,
                                    files_unchanged = r.files_unchanged,
                                    placeholders_gced = r.placeholders_gced,
                                    "cortex reindex: refreshed corpus graph"
                                );
                            }
                            // No source changes (the common case) or corpus not on
                            // this host — nothing to log.
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(corpus = %slug, error = %e, "cortex reindex: pass did not complete");
                            }
                        }
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        tracing::info!("cortex reindex loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::{mode_is_off, parse_corpora_list};

    #[test]
    fn mode_defaults_on_unless_explicit_off() {
        for v in ["off", "OFF", " Off ", "false", "0", "disabled", "no"] {
            assert!(mode_is_off(Some(v)), "{v} should disable");
        }
        for v in [None, Some(""), Some("on"), Some("auto"), Some("whatever")] {
            assert!(!mode_is_off(v), "{v:?} should stay on");
        }
    }

    #[test]
    fn corpora_list_splits_trims_dedups_and_defaults() {
        assert_eq!(
            parse_corpora_list(" forge-fleet , hireflow360 "),
            vec!["forge-fleet", "hireflow360"]
        );
        // Dedup preserves first-seen order; blanks dropped.
        assert_eq!(parse_corpora_list("a,,a, b ,a"), vec!["a", "b"]);
        // Blank / whitespace-only falls back to the self corpus (never a no-op).
        assert_eq!(parse_corpora_list(""), vec!["forge-fleet"]);
        assert_eq!(parse_corpora_list("  , ,"), vec!["forge-fleet"]);
    }
}
