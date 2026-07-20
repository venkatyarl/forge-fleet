//! Train branch operations for ForgeFleet.
//!
//! A "train branch" is a temporary integration branch built by taking a base
//! branch and squashing each queued PR onto it in order. This lets CI validate
//! the combined result of landing several PRs at once.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use thiserror::Error;
use uuid::Uuid;

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Something went wrong while creating a train branch.
#[derive(Debug, Error)]
pub enum TrainBranchError {
    /// The repository path does not exist or is not a directory.
    #[error("repository path is not a directory: {0}")]
    InvalidRepo(PathBuf),

    /// No PRs were supplied.
    #[error("cannot create a train branch from an empty PR queue")]
    EmptyQueue,

    /// A git command failed.
    #[error("git command failed: {message}\nstdout: {stdout}\nstderr: {stderr}")]
    Git {
        message: String,
        stdout: String,
        stderr: String,
    },

    /// A merge conflict prevented the train from being built.
    #[error("merge conflict while integrating PR #{number} ({branch}) on top of {base}")]
    MergeConflict {
        /// PR that could not be merged.
        number: u64,
        /// Branch of the conflicting PR.
        branch: String,
        /// Base branch being built on.
        base: String,
    },
}

// ─── Types ───────────────────────────────────────────────────────────────────

/// A PR queued for inclusion in a train branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPr {
    /// Pull-request number.
    pub number: u64,
    /// Source branch name.
    pub branch: String,
    /// PR title (used in the squash commit message).
    pub title: String,
}

impl QueuedPr {
    /// Create a new queued PR.
    pub fn new(number: u64, branch: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            number,
            branch: branch.into(),
            title: title.into(),
        }
    }
}

/// A successfully created train branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainBranch {
    /// Full name of the created branch.
    pub name: String,
    /// Base branch the train was built from.
    pub base: String,
    /// PRs that were included, in order.
    pub included_prs: Vec<QueuedPr>,
    /// Absolute path to the repository.
    pub repo_path: PathBuf,
}

impl TrainBranch {
    /// Squash commit message used for a given PR.
    fn commit_message(pr: &QueuedPr) -> String {
        format!("PR #{}: {}", pr.number, pr.title)
    }
}

impl fmt::Display for TrainBranch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "train branch '{}' from '{}' with PRs [{}]",
            self.name,
            self.base,
            self.included_prs
                .iter()
                .map(|p| p.number.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

// ─── Git helper ──────────────────────────────────────────────────────────────

fn run_git(repo: &Path, args: &[&str]) -> Result<Output, TrainBranchError> {
    let output = Command::new("git")
        // Ignore any global/user git config so tests and train builds are hermetic.
        .env("GIT_CONFIG_GLOBAL", "")
        .env("GIT_CONFIG_SYSTEM", "")
        .arg("--no-pager")
        .arg("-c")
        .arg("user.name=ForgeFleet Train")
        .arg("-c")
        .arg("user.email=train@forgefleet.local")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| TrainBranchError::Git {
            message: format!("failed to spawn git: {e}"),
            stdout: String::new(),
            stderr: String::new(),
        })?;
    Ok(output)
}

fn require_success(output: Output, context: &str) -> Result<Output, TrainBranchError> {
    if !output.status.success() {
        return Err(TrainBranchError::Git {
            message: context.to_string(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output)
}

// ─── Train branch creation ───────────────────────────────────────────────────

/// Create a temporary train branch from `base` with one squashed commit per PR.
///
/// The function:
/// 1. Validates that `repo_path` is a git repository.
/// 2. Checks out `base`.
/// 3. Creates a new branch named `train/<base>/pr-<n1>-<n2>-...-<short-id>`.
/// 4. Squash-merges each queued PR in order, producing one commit per PR.
///
/// If a merge conflict is encountered, the operation aborts the in-progress
/// merge and returns [`TrainBranchError::MergeConflict`].
pub fn create_train_branch(
    repo_path: impl AsRef<Path>,
    base: impl Into<String>,
    prs: Vec<QueuedPr>,
) -> Result<TrainBranch, TrainBranchError> {
    let repo_path = repo_path.as_ref();
    if !repo_path.is_dir() {
        return Err(TrainBranchError::InvalidRepo(repo_path.to_path_buf()));
    }
    if prs.is_empty() {
        return Err(TrainBranchError::EmptyQueue);
    }

    // Confirm this is a git repository.
    require_success(
        run_git(repo_path, &["rev-parse", "--git-dir"])?,
        "not a git repository",
    )?;

    let base = base.into();

    // Start from the base branch.
    require_success(
        run_git(repo_path, &["checkout", &base])?,
        &format!("failed to checkout base branch '{base}'"),
    )?;

    // Build a deterministic but unique branch name.
    let pr_suffix = prs
        .iter()
        .map(|p| p.number.to_string())
        .collect::<Vec<_>>()
        .join("-");
    let short_id = Uuid::new_v4()
        .to_string()
        .split('-')
        .next()
        .unwrap()
        .to_string();
    let branch_name = format!("train/{}/pr-{}", sanitize_branch_segment(&base), pr_suffix);
    let branch_name = truncate_branch_name(&branch_name, &short_id);

    // Create and check out the train branch.
    require_success(
        run_git(repo_path, &["checkout", "-b", &branch_name])?,
        &format!("failed to create train branch '{branch_name}'"),
    )?;

    // Squash-merge each PR in order.
    for pr in &prs {
        let merge = run_git(repo_path, &["merge", "--squash", "--no-commit", &pr.branch])?;
        if !merge.status.success() {
            // Best-effort abort so the repo is left clean.
            let _ = run_git(repo_path, &["merge", "--abort"]);
            return Err(TrainBranchError::MergeConflict {
                number: pr.number,
                branch: pr.branch.clone(),
                base: base.clone(),
            });
        }

        let message = TrainBranch::commit_message(pr);
        require_success(
            run_git(
                repo_path,
                &["commit", "--no-verify", "--allow-empty", "-m", &message],
            )?,
            &format!(
                "failed to commit squash merge for PR #{} ({})",
                pr.number, pr.branch
            ),
        )?;
    }

    Ok(TrainBranch {
        name: branch_name,
        base,
        included_prs: prs,
        repo_path: repo_path.to_path_buf(),
    })
}

fn sanitize_branch_segment(s: &str) -> String {
    s.replace(['/', ' ', '~', '^', ':', '\\'], "-")
}

fn truncate_branch_name(name: &str, short_id: &str) -> String {
    const MAX_LEN: usize = 200;
    if name.len() <= MAX_LEN {
        return name.to_string();
    }
    let suffix = format!("-{short_id}");
    let kept = &name[..MAX_LEN - suffix.len()];
    format!("{kept}{suffix}")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let repo = dir.path();

        run_git(repo, &["init"]).unwrap();
        run_git(repo, &["checkout", "-b", "main"]).unwrap();

        fs::write(repo.join("README.md"), "# base\n").unwrap();
        run_git(repo, &["add", "README.md"]).unwrap();
        run_git(repo, &["commit", "-m", "Initial commit"]).unwrap();

        dir
    }

    fn create_branch_with_file(
        repo: &Path,
        branch: &str,
        file: &str,
        content: &str,
        message: &str,
    ) {
        run_git(repo, &["checkout", "-b", branch]).unwrap();
        fs::write(repo.join(file), content).unwrap();
        run_git(repo, &["add", file]).unwrap();
        run_git(repo, &["commit", "-m", message]).unwrap();
        run_git(repo, &["checkout", "main"]).unwrap();
    }

    fn commit_messages(repo: &Path, branch: &str) -> Vec<String> {
        let out = run_git(repo, &["log", branch, "--format=%s", "--no-patch"]).unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect()
    }

    fn current_branch(repo: &Path) -> String {
        let out = run_git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn test_create_train_branch_basic() {
        let dir = init_repo();
        let repo = dir.path();

        create_branch_with_file(repo, "feature/a", "a.txt", "a\n", "Add a");
        create_branch_with_file(repo, "feature/b", "b.txt", "b\n", "Add b");

        let prs = vec![
            QueuedPr::new(1, "feature/a", "First feature"),
            QueuedPr::new(2, "feature/b", "Second feature"),
        ];

        let train = create_train_branch(repo, "main", prs.clone()).unwrap();

        assert!(train.name.starts_with("train/main/pr-1-2"));
        assert_eq!(train.base, "main");
        assert_eq!(train.included_prs, prs);
        assert_eq!(train.repo_path, repo);

        // Verify branch exists and is checked out.
        assert_eq!(current_branch(repo), train.name);

        // Verify file contents from both PRs are present.
        assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
        assert_eq!(fs::read_to_string(repo.join("b.txt")).unwrap(), "b\n");

        // Verify commit history: top two commits are the squash merges.
        let messages = commit_messages(repo, &train.name);
        assert_eq!(messages[0], "PR #2: Second feature");
        assert_eq!(messages[1], "PR #1: First feature");
        assert_eq!(messages[2], "Initial commit");
    }

    #[test]
    fn test_train_branch_preserves_order() {
        let dir = init_repo();
        let repo = dir.path();

        // Two independent PRs; the test verifies commit order, not content coupling.
        create_branch_with_file(repo, "pr/1", "line1.txt", "line 1\n", "Add line 1");
        create_branch_with_file(repo, "pr/2", "line2.txt", "line 2\n", "Add line 2");

        let prs = vec![
            QueuedPr::new(10, "pr/1", "Line one"),
            QueuedPr::new(20, "pr/2", "Line two"),
        ];

        let train = create_train_branch(repo, "main", prs).unwrap();
        let messages = commit_messages(repo, &train.name);
        assert_eq!(messages[0], "PR #20: Line two");
        assert_eq!(messages[1], "PR #10: Line one");

        assert_eq!(
            fs::read_to_string(repo.join("line1.txt")).unwrap(),
            "line 1\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("line2.txt")).unwrap(),
            "line 2\n"
        );
    }

    #[test]
    fn test_empty_queue_errors() {
        let dir = init_repo();
        let err = create_train_branch(dir.path(), "main", Vec::new()).unwrap_err();
        assert!(matches!(err, TrainBranchError::EmptyQueue));
    }

    #[test]
    fn test_invalid_repo_errors() {
        let bad = PathBuf::from("/does/not/exist/train-branch-test");
        let err = create_train_branch(&bad, "main", vec![QueuedPr::new(1, "x", "y")]).unwrap_err();
        assert!(matches!(err, TrainBranchError::InvalidRepo(_)));
    }

    #[test]
    fn test_merge_conflict_errors() {
        let dir = init_repo();
        let repo = dir.path();

        create_branch_with_file(repo, "conflict/a", "README.md", "# a\n", "A version");
        create_branch_with_file(repo, "conflict/b", "README.md", "# b\n", "B version");

        let prs = vec![
            QueuedPr::new(1, "conflict/a", "A"),
            QueuedPr::new(2, "conflict/b", "B"),
        ];

        let err = create_train_branch(repo, "main", prs).unwrap_err();
        match err {
            TrainBranchError::MergeConflict {
                number,
                branch,
                base,
            } => {
                assert_eq!(number, 2);
                assert_eq!(branch, "conflict/b");
                assert_eq!(base, "main");
            }
            other => panic!("expected MergeConflict, got {other:?}"),
        }

        // Repo should be back on the attempted train branch (merge aborted cleanly).
        let branch = current_branch(repo);
        assert!(branch.starts_with("train/main"));
    }

    #[test]
    fn test_branch_name_sanitization_and_truncation() {
        assert_eq!(sanitize_branch_segment("feature/test"), "feature-test");
        assert_eq!(sanitize_branch_segment("a b"), "a-b");

        let long = "a".repeat(250);
        let truncated = truncate_branch_name(&format!("train/{long}/pr-1-2-3"), "deadbeef");
        assert!(truncated.len() <= 200);
        assert!(truncated.ends_with("-deadbeef"));
    }
}
