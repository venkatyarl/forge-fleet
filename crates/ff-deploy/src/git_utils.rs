//! Git helpers for deploy-time repository state inspection and mutation.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Return `true` if the git working tree at `repo_path` has tracked modifications.
///
/// Mirrors the leader-local deploy guard in `ff-terminal/src/fleet_cmd.rs`:
/// only tracked changes count; untracked files are deliberately ignored so
/// operator artifacts (`research/`, `graphify-out/`, etc.) do not block a
/// deploy that only builds from tracked sources.
pub fn git_tree_is_dirty(repo_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(repo_path)
        .output()
        .context("failed to spawn git status")?;

    if !output.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

/// Stash tracked modifications in `repo_path` with a recoverable label.
///
/// Returns the label used so callers can surface it in logs or error messages.
/// The stash is reachable via `git stash list` and `refs/stash`.
pub fn git_stash_dirty_tree(repo_path: &Path) -> Result<String> {
    let label = format!(
        "ff-deploy-dirty-guard-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    );

    let output = Command::new("git")
        .args(["stash", "push", "-m", &label])
        .current_dir(repo_path)
        .output()
        .context("failed to spawn git stash")?;

    if !output.status.success() {
        bail!(
            "working tree is dirty and stash failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(label)
}

/// Fetch `origin` and hard-reset to `remote_ref`, first stashing any tracked
/// modifications if the tree is dirty.
///
/// This is the deploy-playbook equivalent of the leader-local guard: the tree
/// is checked before `git reset --hard` runs. If it is dirty, changes are
/// preserved in a labeled stash rather than discarded.
pub fn git_fetch_and_reset_hard(repo_path: &Path, remote_ref: &str) -> Result<()> {
    if git_tree_is_dirty(repo_path)? {
        let label = git_stash_dirty_tree(repo_path)
            .context("working tree is dirty and could not be stashed before reset --hard")?;
        tracing::warn!(
            label = %label,
            path = %repo_path.display(),
            "stashed dirty deploy tree before git reset --hard"
        );
    }

    git_run(repo_path, &["fetch", "origin"])
        .context("failed to fetch origin before reset --hard")?;
    git_run(repo_path, &["reset", "--hard", remote_ref])
        .with_context(|| format!("failed to reset --hard to {remote_ref}"))?;

    Ok(())
}

fn git_run(repo_path: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("failed to spawn git {}", args.join(" ")))?;

    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn init_repo(temp: &std::path::Path) {
        let _ = Command::new("git")
            .args(["init", "-q"])
            .current_dir(temp)
            .output()
            .expect("git init failed");
        let _ = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(temp)
            .output()
            .expect("git config email failed");
        let _ = Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(temp)
            .output()
            .expect("git config name failed");
    }

    #[test]
    fn clean_tree_is_not_dirty() {
        let temp = tempfile::tempdir().unwrap();
        init_repo(temp.path());
        fs::write(temp.path().join("file.txt"), "hello").unwrap();
        let _ = Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        let _ = Command::new("git")
            .args(["commit", "-m", "init", "-q"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        assert!(!git_tree_is_dirty(temp.path()).unwrap());
    }

    #[test]
    fn tracked_modification_is_dirty() {
        let temp = tempfile::tempdir().unwrap();
        init_repo(temp.path());
        fs::write(temp.path().join("file.txt"), "hello").unwrap();
        let _ = Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        let _ = Command::new("git")
            .args(["commit", "-m", "init", "-q"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        fs::write(temp.path().join("file.txt"), "world").unwrap();
        assert!(git_tree_is_dirty(temp.path()).unwrap());
    }

    #[test]
    fn untracked_files_are_ignored() {
        let temp = tempfile::tempdir().unwrap();
        init_repo(temp.path());
        fs::write(temp.path().join("tracked.txt"), "hello").unwrap();
        let _ = Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        let _ = Command::new("git")
            .args(["commit", "-m", "init", "-q"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        fs::write(temp.path().join("untracked.txt"), "ignored").unwrap();
        assert!(!git_tree_is_dirty(temp.path()).unwrap());
    }
}
