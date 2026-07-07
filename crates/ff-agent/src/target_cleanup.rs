//! Per-host build-artifact cleanup.
//!
//! Rust incremental-compile caches (`target/*/incremental`) bloat fast — Taylor
//! hit ~165 GB (task #47). This per-host tick removes the incremental cache under
//! THIS host's forge-fleet build tree once it's gone STALE (untouched > N days,
//! so no build is using it; it regenerates on the next build, so removing it is
//! always safe — it only slightly slows the next incremental step).
//!
//! HARD-SCOPED: it derives the path from `computers.source_tree_path` for THIS
//! host and only ever touches `<src>/target/{debug,release}/incremental`. It can
//! never reach `~/projects/hf360ai`, `hf_cache`, or any training/model dir — the
//! staleness check also means an in-flight build (touching target/ right now) is
//! never disturbed.
//!
//! Per-host (each node cleans its own disk), runs by DEFAULT; opt out with
//! `fleet_secrets.target_cleanup_mode=off`. Window is
//! `fleet_secrets.target_cleanup_days` (default 5, floored at 1).

use anyhow::Result;
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

const MODE_KEY: &str = "target_cleanup_mode";
const DAYS_KEY: &str = "target_cleanup_days";
const DEFAULT_DAYS: u64 = 5;

/// `off`/`false`/`0`/`disabled`/`no` (case-insensitive) disable the tick; any
/// other value — including a missing secret — leaves it ON.
fn mode_is_off(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_lowercase()).as_deref(),
        Some("off" | "false" | "0" | "disabled" | "no")
    )
}

/// Staleness window in days, floored at 1 so a malformed/zero secret can never
/// collapse to 0 and delete a live (currently-building) cache.
fn cleanup_days(v: Option<&str>) -> u64 {
    v.and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_DAYS)
        .max(1)
}

/// Expand a leading `~/` in a path to `$HOME`; returns None if `~/` can't be
/// resolved (no HOME) so callers skip rather than touch a wrong path.
fn expand_home(path: &str) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|h| Path::new(&h).join(rest)),
        None => Some(PathBuf::from(path)),
    }
}

/// True iff `dir` is an existing directory whose mtime is older than `max_age`.
/// A directory touched within the window (e.g. a build in progress) is NOT stale.
fn is_stale(dir: &Path, max_age: Duration) -> bool {
    let Ok(md) = std::fs::metadata(dir) else {
        return false;
    };
    if !md.is_dir() {
        return false;
    }
    match md.modified() {
        Ok(mtime) => SystemTime::now()
            .duration_since(mtime)
            .map(|age| age > max_age)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// The incremental-cache dirs to consider under a build tree. ONLY the
/// `incremental` subdirs — release artifacts + everything else are left intact,
/// so a deploy never eats a full cold rebuild from this.
fn incremental_dirs(src: &Path) -> [PathBuf; 2] {
    [
        src.join("target/debug/incremental"),
        src.join("target/release/incremental"),
    ]
}

/// One cleanup pass on THIS host. Returns the number of stale incremental-cache
/// dirs removed.
pub async fn evaluate_target_cleanup(pg: &PgPool, worker_name: &str) -> Result<usize> {
    if mode_is_off(
        ff_db::pg_get_secret(pg, MODE_KEY)
            .await
            .ok()
            .flatten()
            .as_deref(),
    ) {
        return Ok(0);
    }
    let days = cleanup_days(
        ff_db::pg_get_secret(pg, DAYS_KEY)
            .await
            .ok()
            .flatten()
            .as_deref(),
    );
    let max_age = Duration::from_secs(days * 24 * 3600);

    // Resolve THIS host's forge-fleet build tree; quiet skip if unknown.
    let stp: Option<String> = sqlx::query_scalar(
        "SELECT source_tree_path FROM computers \
          WHERE name = $1 AND NULLIF(source_tree_path, '') IS NOT NULL",
    )
    .bind(worker_name)
    .fetch_optional(pg)
    .await?;
    let Some(src) = stp.and_then(|s| expand_home(&s)) else {
        return Ok(0);
    };

    let mut removed = 0usize;
    for dir in incremental_dirs(&src) {
        if is_stale(&dir, max_age) {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {
                    removed += 1;
                    info!(
                        path = %dir.display(),
                        stale_days = days,
                        "target_cleanup: removed stale incremental cache"
                    );
                }
                Err(e) => {
                    warn!(path = %dir.display(), error = %e, "target_cleanup: remove failed")
                }
            }
        }
    }
    Ok(removed)
}

/// Spawn the per-host target-cleanup loop.
pub fn spawn_target_cleanup_loop(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = evaluate_target_cleanup(&pg, &worker_name).await {
                        warn!(error = %e, "target_cleanup tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("target_cleanup loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_DAYS, cleanup_days, expand_home, incremental_dirs, mode_is_off};
    use std::path::Path;

    #[test]
    fn mode_defaults_on_unless_explicit_off() {
        for v in ["off", "OFF", " Off ", "false", "0", "disabled", "no"] {
            assert!(mode_is_off(Some(v)), "{v} should disable");
        }
        for v in [None, Some(""), Some("on"), Some("keep")] {
            assert!(!mode_is_off(v), "{v:?} should stay on");
        }
    }

    #[test]
    fn window_floors_at_one_day() {
        assert_eq!(cleanup_days(Some("5")), 5);
        assert_eq!(cleanup_days(Some(" 30 ")), 30);
        assert_eq!(cleanup_days(None), DEFAULT_DAYS);
        assert_eq!(cleanup_days(Some("0")), 1);
        assert_eq!(cleanup_days(Some("junk")), DEFAULT_DAYS);
    }

    /// The cleanup targets are ONLY the incremental subdirs under the build
    /// tree's target/ — never the release artifacts, never anything outside
    /// target/. This is the safety invariant that keeps it away from training dirs.
    #[test]
    fn only_touches_incremental_under_the_build_tree() {
        let src = Path::new("/home/w/.forgefleet/sub-agents/sub-agent-0/forge-fleet");
        let dirs = incremental_dirs(src);
        for d in &dirs {
            let s = d.to_string_lossy();
            assert!(
                s.ends_with("/incremental"),
                "must target only incremental: {s}"
            );
            assert!(s.contains("/target/"), "must be under target/: {s}");
        }
        // Never the release binary dir itself, never a parent target/.
        assert!(dirs.iter().all(|d| d != src));
        assert!(
            dirs.iter()
                .all(|d| !d.to_string_lossy().contains("hf360ai"))
        );
    }

    #[test]
    fn expand_home_expands_leading_tilde_only() {
        if let Some(home) = std::env::var_os("HOME") {
            let got = expand_home("~/.forgefleet/x").unwrap();
            assert_eq!(got, Path::new(&home).join(".forgefleet/x"));
        }
        assert_eq!(expand_home("/abs/p").unwrap(), Path::new("/abs/p"));
    }
}
