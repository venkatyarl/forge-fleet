//! Artifact cache directory layout.
//!
//! Constructs platform-scoped artifact cache paths laid out as
//! `~/.forgefleet/cache/<os_family>-<arch>/<name>-<version>.<ext>`.
//!
//! This is a flat, host-keyed tree: one directory per `os_family-arch` pair and
//! one file per artifact version. Cache components are sanitized so
//! user-supplied names and versions cannot escape the cache root.
//!
//! Safety invariants (mirrors `artifact_cache` and `target_cleanup`):
//! - All returned paths stay under the provided cache root.
//! - Components containing path separators, parent-directory references, or
//!   leading dots are normalized to safe single-segment names.
//! - Directory creation is explicit (`ensure_*`) so callers do not create
//!   directories accidentally while merely computing paths.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Canonical local cache root: `~/.forgefleet/cache`.
pub fn default_cache_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".forgefleet")
        .join("cache")
}

/// Detect a simple host OS family key for cache scoping.
///
/// Returns lower-case strings derived from `std::env::consts::OS`, e.g.
/// `"macos"`, `"linux"`, or `"windows"`.
pub fn detect_os_family() -> String {
    match std::env::consts::OS {
        "macos" => "macos".into(),
        "linux" => "linux".into(),
        "windows" => "windows".into(),
        other => other.to_string(),
    }
}

/// Detect the host CPU architecture key for cache scoping.
///
/// Returns the value of `std::env::consts::ARCH`, e.g. `"x86_64"` or
/// `"aarch64"`.
pub fn detect_arch() -> String {
    std::env::consts::ARCH.into()
}

/// Platform-scoped cache directory: `<root>/<os_family>-<arch>`.
pub fn platform_cache_dir(root: impl AsRef<Path>, os_family: &str, arch: &str) -> PathBuf {
    root.as_ref()
        .join(format!("{}-{}", sanitize(os_family), sanitize(arch)))
}

/// Full cache path for one artifact:
/// `<root>/<os_family>-<arch>/<name>-<version>.<ext>`.
pub fn artifact_cache_path(
    root: impl AsRef<Path>,
    os_family: &str,
    arch: &str,
    name: &str,
    version: &str,
    ext: &str,
) -> PathBuf {
    let file_name = format!("{}-{}.{}", sanitize(name), sanitize(version), sanitize(ext));
    platform_cache_dir(root, os_family, arch).join(file_name)
}

/// Create the platform-scoped cache directory, returning its path.
pub fn ensure_platform_cache_dir(
    root: impl AsRef<Path>,
    os_family: &str,
    arch: &str,
) -> Result<PathBuf> {
    let dir = platform_cache_dir(root, os_family, arch);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create platform cache dir {}", dir.display()))?;
    Ok(dir)
}

/// Create the parent directory for an artifact cache file, returning the file path.
pub fn ensure_artifact_cache_path(
    root: impl AsRef<Path>,
    os_family: &str,
    arch: &str,
    name: &str,
    version: &str,
    ext: &str,
) -> Result<PathBuf> {
    let path = artifact_cache_path(root, os_family, arch, name, version, ext);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create artifact cache dir {}", parent.display()))?;
    }
    Ok(path)
}

/// Replace path separators, parent-directory references, and leading dots with
/// `-` so a cache component is always a single safe filename segment.
fn sanitize(s: &str) -> String {
    s.trim()
        .replace(['/', '\\'], "-")
        .replace("..", "-")
        .trim_start_matches('.')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cache_root_uses_forgefleet_cache_tree() {
        let root = default_cache_root();
        assert!(root.ends_with(".forgefleet/cache"));
    }

    #[test]
    fn detect_os_family_is_non_empty() {
        let family = detect_os_family();
        assert!(!family.is_empty());
        assert!(!family.contains('/'));
    }

    #[test]
    fn detect_arch_is_non_empty() {
        let arch = detect_arch();
        assert!(!arch.is_empty());
        assert!(!arch.contains('/'));
    }

    #[test]
    fn platform_cache_dir_joins_os_family_and_arch() {
        let root = Path::new("/tmp/cache");
        let got = platform_cache_dir(root, "linux", "x86_64");
        assert_eq!(got, Path::new("/tmp/cache/linux-x86_64"));
    }

    #[test]
    fn artifact_cache_path_matches_layout() {
        let root = Path::new("/tmp/cache");
        let got = artifact_cache_path(root, "macos", "aarch64", "ff-agent", "v1.2.3", "tar.gz");
        assert_eq!(
            got,
            Path::new("/tmp/cache/macos-aarch64/ff-agent-v1.2.3.tar.gz")
        );
    }

    #[test]
    fn sanitize_normalizes_dangerous_components() {
        assert_eq!(sanitize("../foo"), "--foo");
        assert_eq!(sanitize("a/b"), "a-b");
        assert_eq!(sanitize("a\\b"), "a-b");
        assert_eq!(sanitize(".hidden"), "hidden");
        assert_eq!(sanitize("foo..bar"), "foo-bar");
    }

    #[test]
    fn ensure_platform_cache_dir_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ensure_platform_cache_dir(tmp.path(), "linux", "x86_64").unwrap();
        assert!(dir.exists());
        assert!(dir.is_dir());
        assert!(dir.ends_with("linux-x86_64"));
    }

    #[test]
    fn ensure_artifact_cache_path_creates_parent_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path =
            ensure_artifact_cache_path(tmp.path(), "macos", "aarch64", "ff-agent", "v1", "bin")
                .unwrap();
        assert!(path.parent().unwrap().exists());
        assert_eq!(
            path,
            tmp.path().join("macos-aarch64").join("ff-agent-v1.bin")
        );
    }
}
