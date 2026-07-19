//! Tests for the deploy playbook git dirty-tree handling.
//!
//! These tests exercise the leader-local guard helpers that either stash tracked
//! modifications before a hard reset or refuse to proceed when the tree cannot be
//! safely reconciled.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::Result;
use tempfile::TempDir;

use crate::{git_fetch_and_reset_hard, git_stash_dirty_tree, git_tree_is_dirty};

fn git(repo: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git").args(args).current_dir(repo).output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn init_repo(dir: &Path) -> Result<()> {
    git(dir, &["init", "--quiet", "--initial-branch=main"])?;
    git(dir, &["config", "user.email", "test@forgefleet.local"])?;
    git(dir, &["config", "user.name", "Deploy Test"])?;
    Ok(())
}

fn commit_file(repo: &Path, path: &str, contents: &str) -> Result<()> {
    let full = repo.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&full, contents)?;
    git(repo, &["add", path])?;
    git(repo, &["commit", "--quiet", "-m", &format!("add {}", path)])?;
    Ok(())
}

fn setup_clean_local_repo() -> Result<TempDir> {
    let dir = TempDir::new()?;
    init_repo(dir.path())?;
    commit_file(dir.path(), "tracked.txt", "initial")?;
    Ok(dir)
}

fn setup_bare_remote() -> Result<TempDir> {
    let dir = TempDir::new()?;
    git(
        dir.path(),
        &["init", "--quiet", "--bare", "--initial-branch=main"],
    )?;
    Ok(dir)
}

#[test]
fn clean_tree_reports_not_dirty() {
    let dir = setup_clean_local_repo().expect("repo setup");
    assert!(
        !git_tree_is_dirty(dir.path()).expect("should check tree"),
        "clean tracked tree should not be dirty"
    );
}

#[test]
fn tracked_modification_reports_dirty() {
    let dir = setup_clean_local_repo().expect("repo setup");
    fs::write(dir.path().join("tracked.txt"), "modified").expect("modify tracked file");
    assert!(
        git_tree_is_dirty(dir.path()).expect("should check tree"),
        "tracked modification should be reported as dirty"
    );
}

#[test]
fn untracked_files_are_ignored_by_dirty_check() {
    let dir = setup_clean_local_repo().expect("repo setup");
    fs::write(dir.path().join("untracked.txt"), "ignored").expect("create untracked file");
    assert!(
        !git_tree_is_dirty(dir.path()).expect("should check tree"),
        "untracked files must not count as dirty"
    );
}

#[test]
fn stash_creates_reachable_stash_and_cleans_tree() {
    let dir = setup_clean_local_repo().expect("repo setup");
    fs::write(dir.path().join("tracked.txt"), "modified").expect("modify tracked file");

    let label = git_stash_dirty_tree(dir.path()).expect("stash should succeed");

    assert!(
        !git_tree_is_dirty(dir.path()).expect("should check tree"),
        "tree should be clean after stash"
    );

    let stash_list = Command::new("git")
        .args(["stash", "list"])
        .current_dir(dir.path())
        .output()
        .expect("stash list")
        .stdout;
    let stash_list = String::from_utf8_lossy(&stash_list);
    assert!(
        stash_list.contains(&label),
        "stash list should contain the generated label: {}",
        stash_list
    );
}

#[test]
fn fetch_and_reset_hard_stashes_dirty_tree_before_reset() {
    let remote = setup_bare_remote().expect("remote setup");
    let local = TempDir::new().expect("local temp");

    // Seed the bare remote with an initial commit on main.
    let seed = TempDir::new().expect("seed temp");
    init_repo(seed.path()).expect("init seed");
    git(
        seed.path(),
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    )
    .expect("add remote");
    commit_file(seed.path(), "upstream.txt", "upstream v1").expect("seed commit");
    git(seed.path(), &["push", "--quiet", "origin", "main"]).expect("push seed");

    // Clone the remote to the local repo.
    git(
        local.path(),
        &["clone", "--quiet", remote.path().to_str().unwrap(), "."],
    )
    .expect("clone");
    git(
        local.path(),
        &["config", "user.email", "test@forgefleet.local"],
    )
    .expect("config email");
    git(local.path(), &["config", "user.name", "Deploy Test"]).expect("config name");

    // Add a tracked local modification so the tree is dirty.
    fs::write(local.path().join("upstream.txt"), "local dirty change").expect("dirty file");

    // Push a new commit to origin so reset --hard actually changes HEAD.
    let update = TempDir::new().expect("update temp");
    git(
        update.path(),
        &["clone", "--quiet", remote.path().to_str().unwrap(), "."],
    )
    .expect("clone update");
    commit_file(update.path(), "upstream.txt", "upstream v2").expect("update commit");
    git(update.path(), &["push", "--quiet", "origin", "main"]).expect("push update");

    git_fetch_and_reset_hard(local.path(), "origin/main").expect("reset should succeed");

    assert!(
        !git_tree_is_dirty(local.path()).expect("should check tree"),
        "tree should be clean after dirty reset"
    );

    let head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(local.path())
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .expect("utf8");
    let remote_head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "origin/main"])
            .current_dir(local.path())
            .output()
            .expect("rev-parse origin/main")
            .stdout,
    )
    .expect("utf8");
    assert_eq!(
        head.trim(),
        remote_head.trim(),
        "local HEAD should match origin/main after reset --hard"
    );

    let stash_list = String::from_utf8(
        Command::new("git")
            .args(["stash", "list"])
            .current_dir(local.path())
            .output()
            .expect("stash list")
            .stdout,
    )
    .expect("utf8");
    assert!(
        stash_list.contains("ff-deploy-dirty-guard"),
        "dirty changes should be preserved in a labeled stash"
    );
}

#[test]
fn fetch_and_reset_hard_proceeds_on_clean_tree() {
    let remote = setup_bare_remote().expect("remote setup");
    let local = TempDir::new().expect("local temp");

    // Seed the bare remote.
    let seed = TempDir::new().expect("seed temp");
    init_repo(seed.path()).expect("init seed");
    git(
        seed.path(),
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    )
    .expect("add remote");
    commit_file(seed.path(), "upstream.txt", "upstream v1").expect("seed commit");
    git(seed.path(), &["push", "--quiet", "origin", "main"]).expect("push seed");

    git(
        local.path(),
        &["clone", "--quiet", remote.path().to_str().unwrap(), "."],
    )
    .expect("clone");

    // Push a follow-up commit.
    let update = TempDir::new().expect("update temp");
    git(
        update.path(),
        &["clone", "--quiet", remote.path().to_str().unwrap(), "."],
    )
    .expect("clone update");
    commit_file(update.path(), "upstream.txt", "upstream v2").expect("update commit");
    git(update.path(), &["push", "--quiet", "origin", "main"]).expect("push update");

    git_fetch_and_reset_hard(local.path(), "origin/main")
        .expect("reset should succeed on clean tree");

    assert!(
        !git_tree_is_dirty(local.path()).expect("should check tree"),
        "clean tree should remain clean"
    );

    let head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(local.path())
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .expect("utf8");
    let remote_head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "origin/main"])
            .current_dir(local.path())
            .output()
            .expect("rev-parse origin/main")
            .stdout,
    )
    .expect("utf8");
    assert_eq!(
        head.trim(),
        remote_head.trim(),
        "local HEAD should match origin/main"
    );
}
