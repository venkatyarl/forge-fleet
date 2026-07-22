#!/usr/bin/env bash
# Mirrors scripts/merge-train-success.sh for GitHub Actions workflows that
# invoke scripts out of .github/scripts/. The canonical implementation lives
# in scripts/ so the gh operations never drift between the two.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
exec "$REPO_ROOT/scripts/merge-train-success.sh" "$@"
