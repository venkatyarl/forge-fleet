#!/usr/bin/env bash
# Mirrors scripts/build-train-branch.sh for GitHub Actions workflows that
# invoke scripts out of .github/scripts/. The canonical implementation lives
# in scripts/ so the git operations never drift between the two.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
exec "$REPO_ROOT/scripts/build-train-branch.sh" "$@"
