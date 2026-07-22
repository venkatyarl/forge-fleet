//! Post-run cleanup and leftover detection for work-item dispatch.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const CACHE_TIER_DIRS: &[&str] = &["dl", "devroot", "ossl"];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepResult {
    /// Whether the staging path no longer exists.
    pub staging_removed: bool,
    /// New, non-cache paths relative to `$HOME`.
    pub leftovers: Vec<String>,
    /// Human-readable diagnostics for leftovers or a skipped scan.
    pub warnings: Vec<String>,
}

/// Remove a work item's staging directory and report paths created outside it.
pub fn run_post_sweep(
    staging_path: &Path,
    initial_manifest: &[String],
    work_item: &str,
) -> Result<SweepResult> {
    // Resolve this before deletion because the staging directory may itself be
    // the repository directly beneath the owning slot.
    let slot_root = locate_slot_root(staging_path);

    if staging_path.exists() {
        fs::remove_dir_all(staging_path)
            .with_context(|| format!("rm -rf staging dir {}", staging_path.display()))?;
    }

    let mut result = SweepResult {
        staging_removed: true,
        ..SweepResult::default()
    };
    let Some(home) = dirs::home_dir() else {
        result.warnings.push(format!(
            "work_item {work_item}: could not resolve $HOME; skipped leftover scan"
        ));
        return Ok(result);
    };
    let Some(slot_root) = slot_root else {
        result.warnings.push(format!(
            "work_item {work_item}: staging path {} is not under a sub-agents/sub-agent-<N> slot; skipped leftover scan",
            staging_path.display()
        ));
        return Ok(result);
    };

    let manifest: HashSet<&str> = initial_manifest.iter().map(String::as_str).collect();
    let mut current = list_home_top_level(&home)?;
    walk_slot_tree(&slot_root, &home, &mut current)?;
    current.sort();
    current.dedup();

    for entry in current {
        if manifest.contains(entry.as_str()) || is_cache_tier(&entry) {
            continue;
        }
        result.warnings.push(format!(
            "work_item {work_item}: untracked path left behind after sweep: {entry}"
        ));
        result.leftovers.push(entry);
    }

    Ok(result)
}

fn locate_slot_root(staging_path: &Path) -> Option<PathBuf> {
    staging_path.ancestors().find_map(|ancestor| {
        let name = ancestor.file_name()?.to_str()?;
        let slot = name.strip_prefix("sub-agent-")?;
        let is_numbered_slot = !slot.is_empty() && slot.bytes().all(|b| b.is_ascii_digit());
        let is_under_sub_agents = ancestor
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "sub-agents");
        (is_numbered_slot && is_under_sub_agents).then(|| ancestor.to_path_buf())
    })
}

fn list_home_top_level(home: &Path) -> Result<Vec<String>> {
    let entries = fs::read_dir(home).with_context(|| format!("read_dir {}", home.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read_dir entry under {}", home.display()))?;
        if let Some(name) = entry.file_name().to_str() {
            paths.push(format!("~/{name}"));
        }
    }
    Ok(paths)
}

fn walk_slot_tree(root: &Path, home: &Path, paths: &mut Vec<String>) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("read_dir {}", directory.display()));
            }
        };
        for entry in entries {
            let entry =
                entry.with_context(|| format!("read_dir entry under {}", directory.display()))?;
            let path = entry.path();
            let relative = path
                .strip_prefix(home)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
            paths.push(format!("~/{relative}"));

            let file_type = entry
                .file_type()
                .with_context(|| format!("file_type {}", path.display()))?;
            if file_type.is_dir() && !file_type.is_symlink() {
                pending.push(path);
            }
        }
    }
    Ok(())
}

fn is_cache_tier(entry: &str) -> bool {
    CACHE_TIER_DIRS.iter().any(|directory| {
        let root = format!("~/{directory}");
        entry == root
            || entry
                .strip_prefix(&root)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_numbered_slot_root_at_any_depth() {
        let staging =
            Path::new("/home/x/.forgefleet/sub-agents/sub-agent-3/repo/worktrees/wi-42/abc123");
        assert_eq!(
            locate_slot_root(staging),
            Some(PathBuf::from("/home/x/.forgefleet/sub-agents/sub-agent-3"))
        );
        assert_eq!(
            locate_slot_root(Path::new("/home/x/sub-agent-x/repo")),
            None
        );
    }

    #[test]
    fn filters_only_cache_roots_and_descendants() {
        assert!(is_cache_tier("~/dl"));
        assert!(is_cache_tier("~/dl/model.bin"));
        assert!(is_cache_tier("~/devroot"));
        assert!(is_cache_tier("~/ossl/include"));
        assert!(!is_cache_tier("~/downloads"));
        assert!(!is_cache_tier("~/dl2/model.bin"));
    }

    #[test]
    fn sweep_removes_staging_and_reports_new_slot_paths() {
        let home = tempfile::tempdir().unwrap();
        let slot = home.path().join(".forgefleet/sub-agents/sub-agent-0");
        let staging = slot.join("repo/worktrees/wi-1/abc");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("scratch.txt"), b"temporary").unwrap();
        let leftover = slot.join("repo/build-junk/orphan.o");
        fs::create_dir_all(leftover.parent().unwrap()).unwrap();
        fs::write(&leftover, b"stray").unwrap();
        fs::create_dir_all(home.path().join("dl")).unwrap();

        let previous_home = std::env::var_os("HOME");
        // SAFETY: this test restores HOME before returning and is serialized
        // with other environment-mutating sweep tests by having only one.
        unsafe { std::env::set_var("HOME", home.path()) };
        let result = run_post_sweep(&staging, &[], "wi-1");
        unsafe {
            match previous_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        let result = result.unwrap();

        assert!(result.staging_removed);
        assert!(!staging.exists());
        assert!(
            result
                .leftovers
                .iter()
                .any(|path| path.ends_with("build-junk/orphan.o"))
        );
        assert!(!result.leftovers.iter().any(|path| path == "~/dl"));
        assert!(
            result
                .warnings
                .iter()
                .all(|warning| warning.contains("wi-1"))
        );
    }
}
