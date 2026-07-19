//! Artifact-cache eviction — keep only the latest N versions per artifact.
//!
//! An artifact cache is a directory tree laid out as
//! `<root>/<artifact>/<version>/…` — one subdirectory per artifact, one
//! subdirectory per cached version of that artifact. Versions accumulate
//! forever unless something prunes them, so this janitor tick walks the cache
//! and removes every version beyond the newest `keep_latest` per artifact
//! ("newest" = most recently modified, ties broken by name so a pass is
//! deterministic).
//!
//! Safety invariants (mirrors `target_cleanup` in ff-agent):
//! - HARD-SCOPED: only version directories exactly two levels under `root` are
//!   ever removed. Loose files at either level are never touched, and a missing
//!   root is a quiet no-op.
//! - `keep_latest` is floored at 1, so a malformed/zero config can never wipe
//!   an artifact's only cached version.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::{info, warn};

/// Default number of versions to keep per artifact.
pub const DEFAULT_KEEP_LATEST: usize = 3;

/// Eviction policy for an artifact cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactEvictionPolicy {
    /// How many of the newest versions to keep per artifact.
    keep_latest: usize,
}

impl ArtifactEvictionPolicy {
    /// Build a policy keeping the newest `keep_latest` versions per artifact,
    /// floored at 1 so a zero can never evict every version.
    pub fn new(keep_latest: usize) -> Self {
        Self {
            keep_latest: keep_latest.max(1),
        }
    }

    /// Number of versions kept per artifact.
    pub fn keep_latest(&self) -> usize {
        self.keep_latest
    }
}

impl Default for ArtifactEvictionPolicy {
    fn default() -> Self {
        Self::new(DEFAULT_KEEP_LATEST)
    }
}

/// A cached version of an artifact, as seen on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VersionEntry {
    name: String,
    modified: SystemTime,
}

/// Pick which versions to evict: everything beyond the newest `keep_latest`,
/// newest-first by mtime with name as the deterministic tie-break.
fn select_evictions(mut versions: Vec<VersionEntry>, keep_latest: usize) -> Vec<String> {
    versions.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| b.name.cmp(&a.name))
    });
    versions
        .into_iter()
        .skip(keep_latest.max(1))
        .map(|v| v.name)
        .collect()
}

/// List the version subdirectories of one artifact directory. Loose files are
/// skipped (never eviction candidates); unreadable entries are skipped too.
fn version_dirs(artifact_dir: &Path) -> Vec<VersionEntry> {
    let Ok(entries) = std::fs::read_dir(artifact_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            if !md.is_dir() {
                return None;
            }
            Some(VersionEntry {
                name: e.file_name().to_string_lossy().into_owned(),
                modified: md.modified().ok()?,
            })
        })
        .collect()
}

/// One eviction pass over the cache at `root`. Returns the number of version
/// directories removed. A missing or unreadable root is a quiet no-op.
pub fn evaluate_artifact_eviction(root: &Path, policy: &ArtifactEvictionPolicy) -> Result<usize> {
    let Ok(artifacts) = std::fs::read_dir(root) else {
        return Ok(0);
    };

    let mut removed = 0usize;
    for artifact in artifacts.filter_map(|e| e.ok()) {
        let artifact_dir = artifact.path();
        if !artifact.metadata().map(|md| md.is_dir()).unwrap_or(false) {
            continue;
        }
        for version in select_evictions(version_dirs(&artifact_dir), policy.keep_latest()) {
            let dir = artifact_dir.join(&version);
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {
                    removed += 1;
                    info!(
                        path = %dir.display(),
                        keep_latest = policy.keep_latest(),
                        "artifact_cache: evicted old version"
                    );
                }
                Err(e) => {
                    warn!(path = %dir.display(), error = %e, "artifact_cache: evict failed")
                }
            }
        }
    }
    Ok(removed)
}

/// Spawn the artifact-cache eviction loop.
pub fn spawn_artifact_eviction_loop(
    root: PathBuf,
    policy: ArtifactEvictionPolicy,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = evaluate_artifact_eviction(&root, &policy) {
                        warn!(error = %e, "artifact_cache eviction tick failed");
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("artifact_cache eviction loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn entry(name: &str, secs_ago: u64) -> VersionEntry {
        VersionEntry {
            name: name.to_string(),
            modified: SystemTime::now() - Duration::from_secs(secs_ago),
        }
    }

    #[test]
    fn policy_floors_keep_latest_at_one() {
        assert_eq!(ArtifactEvictionPolicy::new(0).keep_latest(), 1);
        assert_eq!(ArtifactEvictionPolicy::new(5).keep_latest(), 5);
        assert_eq!(
            ArtifactEvictionPolicy::default().keep_latest(),
            DEFAULT_KEEP_LATEST
        );
    }

    #[test]
    fn select_evictions_keeps_newest_n() {
        let versions = vec![entry("v1", 300), entry("v2", 200), entry("v3", 100)];
        assert_eq!(select_evictions(versions.clone(), 2), vec!["v1"]);
        assert_eq!(select_evictions(versions.clone(), 1), vec!["v2", "v1"]);
        assert_eq!(select_evictions(versions, 3), Vec::<String>::new());
    }

    #[test]
    fn select_evictions_never_evicts_everything() {
        // keep_latest 0 is treated as 1 — the newest version always survives.
        let versions = vec![entry("v1", 200), entry("v2", 100)];
        assert_eq!(select_evictions(versions, 0), vec!["v1"]);
    }

    #[test]
    fn select_evictions_ties_break_by_name() {
        let now = SystemTime::now();
        let versions = vec![
            VersionEntry {
                name: "a".into(),
                modified: now,
            },
            VersionEntry {
                name: "b".into(),
                modified: now,
            },
        ];
        // Same mtime — "b" sorts as newer, so "a" is evicted.
        assert_eq!(select_evictions(versions, 1), vec!["a"]);
    }

    #[test]
    fn evict_removes_old_versions_and_spares_files() {
        let root = tempfile::tempdir().unwrap();
        let artifact = root.path().join("ff-agent");
        for v in ["v1", "v2", "v3"] {
            std::fs::create_dir_all(artifact.join(v)).unwrap();
            std::fs::write(artifact.join(v).join("bin"), v).unwrap();
            // Distinct mtimes so v3 is unambiguously the newest.
            std::thread::sleep(Duration::from_millis(15));
        }
        // Loose files at both levels must never be touched.
        std::fs::write(root.path().join("README"), "not an artifact").unwrap();
        std::fs::write(artifact.join("manifest.json"), "not a version").unwrap();

        let removed =
            evaluate_artifact_eviction(root.path(), &ArtifactEvictionPolicy::new(2)).unwrap();
        assert_eq!(removed, 1);
        assert!(!artifact.join("v1").exists());
        assert!(artifact.join("v2").exists());
        assert!(artifact.join("v3").exists());
        assert!(root.path().join("README").exists());
        assert!(artifact.join("manifest.json").exists());
    }

    #[test]
    fn evict_handles_multiple_artifacts_independently() {
        let root = tempfile::tempdir().unwrap();
        for (artifact, versions) in [("alpha", 3usize), ("beta", 1)] {
            for i in 0..versions {
                std::fs::create_dir_all(root.path().join(artifact).join(format!("v{i}"))).unwrap();
                std::thread::sleep(Duration::from_millis(15));
            }
        }

        let removed =
            evaluate_artifact_eviction(root.path(), &ArtifactEvictionPolicy::new(1)).unwrap();
        assert_eq!(removed, 2);
        assert!(root.path().join("alpha").join("v2").exists());
        assert!(!root.path().join("alpha").join("v1").exists());
        assert!(!root.path().join("alpha").join("v0").exists());
        assert!(root.path().join("beta").join("v0").exists());
    }

    #[test]
    fn evict_missing_root_is_noop() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("does-not-exist");
        let removed =
            evaluate_artifact_eviction(&missing, &ArtifactEvictionPolicy::default()).unwrap();
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn eviction_loop_stops_on_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = spawn_artifact_eviction_loop(
            root.path().to_path_buf(),
            ArtifactEvictionPolicy::default(),
            3600,
            rx,
        );
        tx.send(true).unwrap();
        handle.await.unwrap();
    }
}
