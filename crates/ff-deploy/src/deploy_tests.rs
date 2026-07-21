use std::fs;
use std::path::Path;
use std::process::Command;

use crate::deploy::{git_fetch_and_reset_hard, git_tree_is_dirty};

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("failed to run git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn init_origin() -> tempfile::TempDir {
    let origin = tempfile::tempdir().unwrap();
    git(origin.path(), &["init", "-q"]);
    git(origin.path(), &["config", "user.email", "test@example.com"]);
    git(origin.path(), &["config", "user.name", "Test"]);
    fs::write(origin.path().join("source.rs"), "committed\n").unwrap();
    git(origin.path(), &["add", "source.rs"]);
    git(origin.path(), &["commit", "-m", "initial", "-q"]);
    git(origin.path(), &["branch", "-M", "main"]);
    origin
}

fn clone_origin(origin: &Path) -> tempfile::TempDir {
    let checkout = tempfile::tempdir().unwrap();
    git(
        checkout.path(),
        &["clone", "-q", origin.to_str().unwrap(), "."],
    );
    checkout
}

#[test]
fn deploy_stashes_dirty_source_tree_before_reset() {
    let origin = init_origin();
    let checkout = clone_origin(origin.path());
    fs::write(checkout.path().join("source.rs"), "local change\n").unwrap();

    git_fetch_and_reset_hard(checkout.path(), "origin/main").unwrap();

    assert!(!git_tree_is_dirty(checkout.path()).unwrap());
    assert_eq!(
        fs::read_to_string(checkout.path().join("source.rs")).unwrap(),
        "committed\n"
    );
    assert!(git(checkout.path(), &["stash", "show", "-p"]).contains("local change"));
}

#[test]
fn deploy_refuses_dirty_source_tree_when_stash_fails() {
    let origin = init_origin();
    let checkout = clone_origin(origin.path());
    fs::write(checkout.path().join("source.rs"), "must survive\n").unwrap();

    // An existing index lock makes `git stash` fail while `git status` can
    // still detect the dirty tree. The deploy must stop before fetch/reset.
    fs::write(checkout.path().join(".git/index.lock"), "locked").unwrap();
    let error = git_fetch_and_reset_hard(checkout.path(), "origin/main").unwrap_err();

    assert!(error.to_string().contains("could not be stashed"));
    assert_eq!(
        fs::read_to_string(checkout.path().join("source.rs")).unwrap(),
        "must survive\n"
    );
    assert!(git_tree_is_dirty(checkout.path()).unwrap());
}
