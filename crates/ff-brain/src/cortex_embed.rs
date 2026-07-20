//! Cortex semantic-embedding pass.
//!
//! Cortex (`ff cortex index`) builds the STRUCTURAL graph — `code:*` / `doc:*` /
//! `data:*` / `image:*` nodes in `brain_vault_nodes` plus their edges. It never
//! populated the `embedding vector(1024)` column, so semantic search over the
//! Cortex graph (`vector_search` / `hybrid_search`) returned nothing: those
//! helpers embed the *query* and compare against stored node vectors, of which
//! there were zero.
//!
//! This module fills that column. It discovers a live fleet embedding endpoint
//! (bge-m3, 1024-dim) via [`fleet_embedding_client`] and batch-embeds the node
//! identity text (`<kind> <fully-qualified-title> [tags]`). It deliberately
//! ABORTS when no real endpoint exists rather than fall through to the hash
//! stub — storing deterministic-noise vectors would silently poison search.
//!
//! Community detection ([`crate::detect_communities`]) is a separate, cheap
//! graph pass the caller runs afterwards; the two together make the Cortex
//! graph navigable the way graphify / legacy code-graph tools are.

use sqlx::{PgPool, Row};

use crate::embeddings::fleet_embedding_client;
use crate::vector_search::embedding_to_pgvector;

/// How many node texts to send per `/v1/embeddings` request. bge-m3 handles a
/// few hundred short strings comfortably; 64 keeps each request small and the
/// progress log lively without thrashing the endpoint.
const EMBED_BATCH: usize = 64;

/// Node types Cortex owns — the ones this pass embeds. Vault notes / facts are
/// embedded by their own ingestion path and are left alone here.
const CORTEX_PREFIXES: &[&str] = &["code:", "doc:", "data:", "image:"];

/// Bail on the embedding pass after this many CONSECUTIVE batch failures —
/// enough to ride out a transient blip (a re-routed endpoint, a brief model
/// eviction) but not so many that a genuinely-down endpoint spins for long.
const MAX_CONSECUTIVE_BATCH_FAILURES: u32 = 5;

/// Maximum number of batches to process before returning, to prevent
/// over-processing in long-running embed passes.
const MAX_ITERATIONS: usize = 50;

/// Seconds to wait before retrying after the Nth consecutive batch failure:
/// linear `2·n` so a one-off blip pauses briefly while a persistent outage
/// still trips the bail threshold quickly (2+4+6+8 = 20s across 4 retries).
/// Pure for unit testing.
fn batch_retry_backoff_secs(consecutive_failures: u32) -> u64 {
    2 * consecutive_failures as u64
}

/// Push a node that failed to embed to the back of the `ORDER BY updated_at`
/// queue (without setting an embedding) so the batch scan advances past it
/// instead of re-fetching the same failing row at the front forever. The node
/// stays NULL, so a later `ff cortex embed` re-run still retries it. Best-effort
/// — a failure to bump is non-fatal (worst case the row is retried this run).
async fn defer_failed_node(pool: &PgPool, id: &uuid::Uuid) {
    let _ = sqlx::query("UPDATE brain_vault_nodes SET updated_at = NOW() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await;
}

/// Outcome of an embedding pass.
#[derive(Debug, Default, Clone)]
pub struct EmbedStats {
    /// Nodes that got a fresh embedding stored this pass.
    pub embedded: usize,
    /// Nodes whose embedding call failed (left NULL for a later pass).
    pub failed: usize,
    /// Cortex nodes still NULL after the pass (e.g. a `--max` cap was hit).
    pub remaining: i64,
}

/// Build the text we embed for one node. The fully-qualified title
/// (`ff_pulse::heartbeat::start`) already encodes crate + module + symbol, so a
/// short kind prefix + tags is enough to make symbol-name semantic search work
/// ("where do we publish heartbeats" → `publish_beat`). Doc sections carry a
/// human title which embeds directly.
fn embed_text(node_type: &str, title: &str, tags: &[String]) -> String {
    let kind = node_type.split(':').next_back().unwrap_or(node_type);
    if tags.is_empty() {
        format!("{kind} {title}")
    } else {
        format!("{kind} {title} [{}]", tags.join(", "))
    }
}

/// Count Cortex nodes still missing an embedding. When `corpus` is `Some`, only
/// nodes whose `project` matches that corpus slug are counted (NULL = fleet-wide).
async fn remaining_unembedded(pool: &PgPool, corpus: Option<&str>) -> Result<i64, String> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM brain_vault_nodes
          WHERE valid_until IS NULL AND embedding IS NULL
            AND (node_type LIKE 'code:%' OR node_type LIKE 'doc:%'
              OR node_type LIKE 'data:%' OR node_type LIKE 'image:%')
            AND ($1::text IS NULL OR project = $1)",
    )
    .bind(corpus)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("count unembedded: {e}"))?;
    Ok(n)
}

/// Embed every Cortex node whose `embedding` is NULL, in batches, until the
/// graph is fully embedded or `max` nodes have been processed this run.
///
/// `progress` is invoked after each batch with `(embedded_so_far, remaining)`
/// so the CLI can render a live counter. Returns once no unembedded Cortex
/// nodes remain (or the cap is hit). Aborts immediately if the fleet has no
/// healthy embedding endpoint — by design, to avoid persisting hash-stub noise.
///
/// `corpus` scopes the pass to a single corpus slug (the `project` column). The
/// fleet-wide pass embeds by `updated_at` order, so a freshly-reindexed corpus
/// (newest rows) is embedded LAST — passing its slug here lets an agent embed
/// the repo it's working in first, instead of waiting behind every other corpus.
pub async fn embed_cortex_nodes<F>(
    pool: &PgPool,
    max: Option<usize>,
    corpus: Option<&str>,
    mut progress: F,
) -> Result<EmbedStats, String>
where
    F: FnMut(usize, i64),
{
    let client = fleet_embedding_client(pool).await.ok_or_else(|| {
        "no healthy fleet embedding endpoint — load one with \
         `ff model load <bge-m3-lib-id>` (needs preferred_workloads=embedding)"
            .to_string()
    })?;

    let mut stats = EmbedStats::default();
    // A transient endpoint blip (network hiccup, a brief model eviction, one
    // 500 on an oversized batch) used to abort the WHOLE pass on the first
    // error — on a 50k-node run that abandoned every node after the blip. We
    // now retry the same batch with backoff and only bail once the endpoint is
    // persistently down (this many consecutive failures).
    let mut consecutive_failures: u32 = 0;

    // Upper bound on nodes we'll attempt this run. Each NULL node is either
    // embedded (drops out of the fetch) or counted failed + deferred to the
    // back of the queue — but a node the endpoint permanently rejects (e.g.
    // text too long) stays NULL forever, so without this cap the deferred set
    // would be re-fetched endlessly once it's all that remains. Attempting at
    // most `initial_remaining` nodes guarantees termination; a re-run retries
    // the deferred failures.
    let initial_remaining = remaining_unembedded(pool, corpus).await.unwrap_or(i64::MAX);

    let mut iterations = 0;
    loop {
        iterations += 1;
        if iterations > MAX_ITERATIONS {
            tracing::warn!(
                max_iterations = MAX_ITERATIONS,
                "cortex embed iteration cap reached; stopping pass"
            );
            break;
        }

        if stats.embedded + stats.failed >= initial_remaining as usize {
            break;
        }
        if let Some(cap) = max {
            if stats.embedded + stats.failed >= cap {
                break;
            }
        }

        // Pull a batch of still-NULL Cortex nodes. The WHERE clause is stable
        // across iterations because each row we touch is set non-NULL (or
        // counted as failed and retried next run), so the window advances.
        let rows = sqlx::query(
            "SELECT id, node_type, title, tags FROM brain_vault_nodes
              WHERE valid_until IS NULL AND embedding IS NULL
                AND (node_type LIKE 'code:%' OR node_type LIKE 'doc:%'
                  OR node_type LIKE 'data:%' OR node_type LIKE 'image:%')
                AND ($2::text IS NULL OR project = $2)
              ORDER BY updated_at
              LIMIT $1",
        )
        .bind(EMBED_BATCH as i64)
        .bind(corpus)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("fetch unembedded batch: {e}"))?;

        if rows.is_empty() {
            break;
        }

        // Decode the batch.
        let mut ids: Vec<uuid::Uuid> = Vec::with_capacity(rows.len());
        let mut texts: Vec<String> = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: uuid::Uuid = r.get("id");
            let node_type: String = r.try_get("node_type").unwrap_or_default();
            let title: String = r.try_get("title").unwrap_or_default();
            let tags: Vec<String> = r.try_get("tags").unwrap_or_default();
            ids.push(id);
            texts.push(embed_text(&node_type, &title, &tags));
        }

        let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let vectors = match client.embed_batch(&text_refs).await {
            Ok(v) if v.len() == ids.len() => v,
            Ok(v) => {
                // Length mismatch — a malformed response, treated like a batch
                // failure: retry with backoff (don't advance), bail if it
                // persists. The earlier `continue` here re-fetched the SAME rows
                // forever on a persistently-bad batch (latent wedge).
                tracing::warn!(
                    expected = ids.len(),
                    got = v.len(),
                    "embedding batch length mismatch"
                );
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_BATCH_FAILURES {
                    stats.failed += ids.len();
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(batch_retry_backoff_secs(
                    consecutive_failures,
                )))
                .await;
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    attempt = consecutive_failures + 1,
                    "embedding batch failed: {e}"
                );
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_BATCH_FAILURES {
                    // Persistently down — bail so the caller sees partial
                    // progress (re-run resumes; only NULL nodes are touched).
                    stats.failed += ids.len();
                    break;
                }
                // Transient: back off and retry the SAME rows rather than
                // abandoning the rest of the pass.
                tokio::time::sleep(std::time::Duration::from_secs(batch_retry_backoff_secs(
                    consecutive_failures,
                )))
                .await;
                continue;
            }
        };
        // The endpoint answered a full batch — reset the transient-failure run.
        consecutive_failures = 0;

        for (id, vec) in ids.iter().zip(vectors.iter()) {
            if vec.is_empty() {
                stats.failed += 1;
                // The endpoint returned no vector for this node (e.g. text too
                // long). Push it to the back of the queue so the `ORDER BY
                // updated_at` scan advances instead of re-fetching it at the
                // front forever; it stays NULL, so a later re-run still retries.
                defer_failed_node(pool, id).await;
                continue;
            }
            let pgvec = embedding_to_pgvector(vec);
            match sqlx::query(
                "UPDATE brain_vault_nodes SET embedding = $1::vector, updated_at = NOW() WHERE id = $2",
            )
            .bind(&pgvec)
            .bind(id)
            .execute(pool)
            .await
            {
                Ok(_) => {
                    stats.embedded += 1;
                    crate::cortex::storage::mirror_embedding(pool, *id, vec).await;
                }
                Err(e) => {
                    tracing::warn!(node = %id, "store embedding failed: {e}");
                    stats.failed += 1;
                    defer_failed_node(pool, id).await;
                }
            }
        }

        let remaining = remaining_unembedded(pool, corpus).await.unwrap_or(-1);
        progress(stats.embedded, remaining);
    }

    stats.remaining = remaining_unembedded(pool, corpus).await.unwrap_or(-1);
    Ok(stats)
}

// ── Automated embed-refresh tick ───────────────────────────────────────────
//
// `ff cortex index` keeps the STRUCTURAL graph current (the git hook re-indexes
// on every commit), but a freshly-(re)indexed code symbol lands with a NULL
// `embedding`, so `find --semantic` goes stale on just-changed code until
// someone manually runs `ff cortex embed`. This leader-gated daemon tick drains
// the unembedded backlog automatically — pure maintenance over the `embedding`
// column (no fleet serving state is mutated), so it defaults ON like the other
// maintenance ticks (log rotation, orphan reaper, disk sampler) rather than the
// state-mutating ticks (autoscaler, disk-reconcile) that default OFF.

/// `fleet_secrets` key holding the kill-switch for the embed-refresh tick.
const EMBED_REFRESH_MODE_KEY: &str = "cortex_embed_mode";

/// Default cap on nodes embedded per tick. Keeps each hourly pass a small, bounded
/// load on the fleet bge-m3 endpoint while still draining the backlog over time;
/// the real-time case (a handful of code symbols a commit just changed) clears in
/// the first tick. Override with `FORGEFLEET_CORTEX_EMBED_MAX_PER_TICK`.
const DEFAULT_EMBED_MAX_PER_TICK: usize = 5000;

/// The tick's two-state gate. Pure maintenance, so it runs by DEFAULT — an
/// operator opts OUT by setting `fleet_secrets.cortex_embed_mode=off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedRefreshMode {
    /// Drain the unembedded backlog each tick (default).
    On,
    /// Disabled — the tick is a pure no-op.
    Off,
}

impl EmbedRefreshMode {
    /// Parse the gate value. Missing / empty / unrecognised → `On` (the default);
    /// only an explicit off-like value disables it.
    fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("off") | Some("false") | Some("0") | Some("disabled") | Some("no") => {
                EmbedRefreshMode::Off
            }
            // On, missing, empty, "auto", or any other value → run by default.
            _ => EmbedRefreshMode::On,
        }
    }
}

/// Read the per-tick cap from `FORGEFLEET_CORTEX_EMBED_MAX_PER_TICK`, falling back
/// to [`DEFAULT_EMBED_MAX_PER_TICK`]. A non-positive / unparseable value uses the
/// default; `0` is treated as "use the default" (never an unbounded pass).
fn embed_max_per_tick() -> usize {
    std::env::var("FORGEFLEET_CORTEX_EMBED_MAX_PER_TICK")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_EMBED_MAX_PER_TICK)
}

/// Read the kill-switch from `fleet_secrets`. Defaults to `On` when the key is
/// missing or unreadable — shipping the tick keeps `find --semantic` fresh.
async fn read_refresh_mode(pool: &PgPool) -> EmbedRefreshMode {
    match ff_db::pg_get_secret(pool, EMBED_REFRESH_MODE_KEY).await {
        Ok(v) => EmbedRefreshMode::parse(v.as_deref()),
        Err(e) => {
            tracing::warn!(error = %e, "cortex embed-refresh: failed to read mode secret; defaulting on");
            EmbedRefreshMode::On
        }
    }
}

/// Spawn the leader-gated cortex embed-refresh loop. Mirrors the procedural-memory
/// consolidation loop: fire on the interval, skip unless this node is the live
/// leader, then drain up to `embed_max_per_tick()` unembedded Cortex nodes. The
/// embed pass itself bails gracefully when no fleet embedding endpoint is live
/// (so a fleet with no bge-m3 loaded just logs and waits), is fully resumable,
/// and only touches NULL rows — so a tick that overlaps a manual `ff cortex embed`
/// is harmless.
pub fn spawn_embed_refresh_loop(
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

                    if read_refresh_mode(&pg).await == EmbedRefreshMode::Off {
                        continue;
                    }

                    // Nothing to do? Skip the endpoint discovery + log noise.
                    match remaining_unembedded(&pg, None).await {
                        Ok(0) => continue,
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "cortex embed-refresh: count failed; skipping tick");
                            continue;
                        }
                    }

                    let cap = embed_max_per_tick();
                    match embed_cortex_nodes(&pg, Some(cap), None, |_, _| {}).await {
                        Ok(stats) => {
                            if stats.embedded > 0 || stats.failed > 0 {
                                tracing::info!(
                                    embedded = stats.embedded,
                                    failed = stats.failed,
                                    remaining = stats.remaining,
                                    cap,
                                    "cortex embed-refresh: drained unembedded nodes"
                                );
                            }
                        }
                        Err(e) => {
                            // No live endpoint / persistent outage — expected when no
                            // bge-m3 is loaded. Resumes next tick once one comes up.
                            tracing::warn!(error = %e, "cortex embed-refresh: pass did not complete");
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
        tracing::info!("cortex embed-refresh loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::{EmbedRefreshMode, embed_text};

    #[test]
    fn refresh_mode_defaults_on_when_missing_or_unknown() {
        assert_eq!(EmbedRefreshMode::parse(None), EmbedRefreshMode::On);
        assert_eq!(EmbedRefreshMode::parse(Some("")), EmbedRefreshMode::On);
        assert_eq!(EmbedRefreshMode::parse(Some("  ")), EmbedRefreshMode::On);
        assert_eq!(EmbedRefreshMode::parse(Some("on")), EmbedRefreshMode::On);
        assert_eq!(EmbedRefreshMode::parse(Some("auto")), EmbedRefreshMode::On);
        assert_eq!(
            EmbedRefreshMode::parse(Some("whatever")),
            EmbedRefreshMode::On
        );
    }

    #[test]
    fn refresh_mode_off_only_for_explicit_off_values() {
        for v in ["off", "OFF", " Off ", "false", "0", "disabled", "no"] {
            assert_eq!(
                EmbedRefreshMode::parse(Some(v)),
                EmbedRefreshMode::Off,
                "{v:?} should disable"
            );
        }
    }

    #[test]
    fn embed_text_without_tags_is_kind_plus_title() {
        // Leaf kind only (after the last ':'), then the fully-qualified title.
        let t = embed_text("code:function", "ff_pulse::heartbeat::start", &[]);
        assert_eq!(t, "function ff_pulse::heartbeat::start");
    }

    #[test]
    fn embed_text_with_tags_appends_bracketed_list() {
        let t = embed_text(
            "code:function",
            "ff_db::pg_reprofile_candidates",
            &["agent".to_string(), "tool_calling".to_string()],
        );
        assert_eq!(
            t,
            "function ff_db::pg_reprofile_candidates [agent, tool_calling]"
        );
    }

    #[test]
    fn embed_text_unprefixed_node_type_kept_whole() {
        // A node_type with no ':' falls back to itself as the kind.
        let t = embed_text("doc", "Cortex roadmap", &[]);
        assert_eq!(t, "doc Cortex roadmap");
    }

    #[test]
    fn batch_retry_backoff_grows_then_caps_at_bail() {
        use super::{MAX_CONSECUTIVE_BATCH_FAILURES, batch_retry_backoff_secs};
        // Linear growth across retries, no wait before the first attempt.
        assert_eq!(batch_retry_backoff_secs(0), 0);
        assert_eq!(batch_retry_backoff_secs(1), 2);
        assert_eq!(batch_retry_backoff_secs(4), 8);
        // We only ever sleep for failures 1..MAX-1 (MAX bails), so the longest
        // backoff actually used is for the (MAX-1)th failure — bounded + small.
        let last_used = batch_retry_backoff_secs(MAX_CONSECUTIVE_BATCH_FAILURES - 1);
        assert!(last_used <= 30, "backoff must stay small: {last_used}s");
    }
}
