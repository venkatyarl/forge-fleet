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
//! Leader-gated (only the leader's dev checkout drives the canonical corpus),
//! hourly, runs by DEFAULT; opt out with `fleet_secrets.cortex_index_mode=off`.
//! Self-corpus only for now — multi-corpus/multi-lang is a follow-up.

use anyhow::Result;
use sqlx::PgPool;

use crate::{corpus, cortex};

/// The corpus the daemon keeps fresh: ForgeFleet's own source tree — the graph
/// agents query about ff itself.
const SELF_CORPUS: &str = "forge-fleet";
/// Language re-parsed each tick (forge-fleet is overwhelmingly Rust).
const SELF_LANGS: &[&str] = &["rust"];
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

/// Re-scan + incrementally re-index the self corpus. Returns the incremental
/// report, or `None` when the corpus isn't registered on this host (its source
/// checkout lives elsewhere) — a quiet skip, not an error.
pub async fn reindex_self_corpus(pool: &PgPool) -> Result<Option<cortex::IncrementalReport>> {
    let Some(c) = corpus::get_corpus(pool, SELF_CORPUS).await? else {
        return Ok(None);
    };
    // Refresh the corpus content:file nodes from disk (the incremental indexer
    // diffs against these — without a re-scan it would never see new files).
    corpus::scan(pool, &c, None, 12).await?;
    let langs: Vec<String> = SELF_LANGS.iter().map(|s| s.to_string()).collect();
    let report = cortex::index_langs_incremental(pool, SELF_CORPUS, &langs).await?;
    Ok(Some(report))
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

                    match reindex_self_corpus(&pg).await {
                        Ok(Some(r)) if r.files_changed > 0 || r.files_deleted > 0 => {
                            tracing::info!(
                                files_changed = r.files_changed,
                                files_deleted = r.files_deleted,
                                files_unchanged = r.files_unchanged,
                                placeholders_gced = r.placeholders_gced,
                                "cortex reindex: refreshed the self corpus graph"
                            );
                        }
                        // No source changes (the common case) or corpus not on this
                        // host — nothing to log.
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "cortex reindex: pass did not complete");
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
    use super::mode_is_off;

    #[test]
    fn mode_defaults_on_unless_explicit_off() {
        for v in ["off", "OFF", " Off ", "false", "0", "disabled", "no"] {
            assert!(mode_is_off(Some(v)), "{v} should disable");
        }
        for v in [None, Some(""), Some("on"), Some("auto"), Some("whatever")] {
            assert!(!mode_is_off(v), "{v:?} should stay on");
        }
    }
}
