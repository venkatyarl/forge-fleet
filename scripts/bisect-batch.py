#!/usr/bin/env python3
"""Bisect a failed train branch to find the first PR that broke `make test`.

A train branch (see crates/ff-orchestrator/src/train_branch.rs) is built by
squash-merging queued PRs onto a base branch in order, one commit per PR with
subject "PR #<n>: <title>". When the train's validation fails, this script
reads the queued PR numbers off that commit history and binary searches them:
for each midpoint it checks out the base branch, squash-merges PRs 1..=mid
(resolving each PR's head branch via `gh pr view`), and runs `make test` —
mirroring the invocation/exit-code check used by the T1 fast-check pipeline
(crates/ff-pipeline/src/testing_pipeline.rs: `o.status.success()`).

Usage:
    scripts/bisect-batch.py --train-branch train/main/pr-12-13-14-abcd123 [--base main]
"""

import argparse
import re
import subprocess
import sys

PR_SUBJECT_RE = re.compile(r"^PR #(\d+): ")


def log(message):
    print(f"[bisect] {message}", file=sys.stderr)


def run_git(repo, args):
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True)


def require_success(result, context):
    if result.returncode != 0:
        sys.exit(f"{context}\nstdout: {result.stdout}\nstderr: {result.stderr}")
    return result


def current_branch(repo):
    out = require_success(
        run_git(repo, ["rev-parse", "--abbrev-ref", "HEAD"]), "failed to read current branch"
    )
    return out.stdout.strip()


def queued_prs_from_train_branch(repo, train_branch, base):
    """Return the PR numbers queued on `train_branch`, oldest (first-merged) first."""
    out = require_success(
        run_git(repo, ["log", f"{base}..{train_branch}", "--format=%s", "--no-patch"]),
        f"failed to read commit log for train branch '{train_branch}' since '{base}'",
    )
    numbers = []
    for subject in reversed(out.stdout.splitlines()):  # git log is newest-first
        match = PR_SUBJECT_RE.match(subject)
        if match:
            numbers.append(int(match.group(1)))
    if not numbers:
        sys.exit(f"no 'PR #<n>: ...' commits found on '{train_branch}' since '{base}'")
    return numbers


def pr_head_branch(pr_number):
    result = subprocess.run(
        ["gh", "pr", "view", str(pr_number), "--json", "headRefName", "-q", ".headRefName"],
        capture_output=True,
        text=True,
    )
    require_success(result, f"gh pr view failed for PR #{pr_number}")
    return result.stdout.strip()


SCRATCH_BRANCH = "ff-bisect-scratch"


def squash_prs_onto_base(repo, base, pr_numbers):
    """Reset a scratch branch to `base` and squash-merge each PR in
    `pr_numbers` onto it, in order. Commits land on the scratch branch, never
    on `base` itself, so repeated bisect iterations start from a clean base."""
    require_success(
        run_git(repo, ["checkout", "-B", SCRATCH_BRANCH, base]),
        f"failed to reset scratch branch onto base '{base}'",
    )
    for number in pr_numbers:
        branch = pr_head_branch(number)
        merge = run_git(repo, ["merge", "--squash", "--no-commit", branch])
        if merge.returncode != 0:
            run_git(repo, ["merge", "--abort"])
            sys.exit(f"merge conflict integrating PR #{number} ({branch}) onto '{base}'")
        require_success(
            run_git(repo, ["commit", "--no-verify", "--allow-empty", "-m", f"PR #{number}"]),
            f"failed to commit squash merge for PR #{number} ({branch})",
        )


def run_test_command(repo):
    result = subprocess.run(["make", "test"], cwd=repo, capture_output=True, text=True)
    passed = result.returncode == 0
    return passed, result


def bisect(repo, base, pr_numbers):
    """Binary search for the first PR (by queue position) whose inclusion makes
    `make test` fail. Assumes failure is monotonic in queue order: once a
    midpoint fails, every PR after it in the queue also fails."""
    lo, hi = 0, len(pr_numbers) - 1
    first_failing = None
    while lo <= hi:
        mid = (lo + hi) // 2
        candidate_prs = pr_numbers[: mid + 1]
        log(f"testing PRs up to #{pr_numbers[mid]} ({mid + 1}/{len(pr_numbers)})")
        squash_prs_onto_base(repo, base, candidate_prs)
        passed, result = run_test_command(repo)
        log(f"make test -> {'PASS' if passed else 'FAIL'} (exit {result.returncode})")
        if passed:
            lo = mid + 1
        else:
            first_failing = pr_numbers[mid]
            hi = mid - 1
    return first_failing


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repo", default=".", help="path to the git repository (default: cwd)")
    parser.add_argument("--train-branch", help="the failed train branch to bisect (default: current branch)")
    parser.add_argument("--base", default="main", help="base branch the train was built from (default: main)")
    args = parser.parse_args()

    train_branch = args.train_branch or current_branch(args.repo)
    pr_numbers = queued_prs_from_train_branch(args.repo, train_branch, args.base)
    log(f"queued PRs on '{train_branch}': {pr_numbers}")

    first_failing = bisect(args.repo, args.base, pr_numbers)

    require_success(run_git(args.repo, ["checkout", args.base]), f"failed to restore '{args.base}'")
    run_git(args.repo, ["branch", "-D", SCRATCH_BRANCH])

    if first_failing is None:
        log("no failing PR found -- all candidates passed `make test`")
        sys.exit(0)

    log(f"first failing PR: #{first_failing}")
    print(first_failing)


if __name__ == "__main__":
    main()
