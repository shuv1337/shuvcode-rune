#!/usr/bin/env bash
# Regular upstream sync helper for shuv1337/shuvcode-rune (fork of anomalyco/opencode-rune).
#
# Usage:
#   scripts/fork-sync.sh           Fetch upstream/dev and report incoming commits + fork status
#   scripts/fork-sync.sh --merge   Fetch upstream/dev, verify clean tree, and merge into current branch
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

mode="status"
if [[ "${1:-}" == "--merge" ]]; then
  mode="merge"
elif [[ -n "${1:-}" ]]; then
  echo "Usage: scripts/fork-sync.sh [--merge]" >&2
  exit 2
fi

if ! git remote get-url upstream >/dev/null 2>&1; then
  echo "fork-sync: missing 'upstream' remote (expected https://github.com/anomalyco/opencode-rune.git)" >&2
  exit 1
fi

echo "==> Fetching upstream/dev and pruning origin"
git fetch upstream dev
git fetch --prune origin

branch="$(git branch --show-current)"
head_sha="$(git rev-parse --short=9 HEAD)"
upstream_sha="$(git rev-parse --short=9 upstream/dev)"
merge_base="$(git merge-base HEAD upstream/dev)"
behind="$(git rev-list --count HEAD..upstream/dev)"
ahead="$(git rev-list --count upstream/dev..HEAD)"

echo "Branch:    ${branch:-DETACHED} ($head_sha)"
echo "Upstream:  upstream/dev ($upstream_sha)"
echo "MergeBase: ${merge_base:0:9}"
echo "Divergence: ahead $ahead / behind $behind"

unmerged_local="$(git branch --no-merged HEAD | sed 's/^[* ]*//' | grep -v '^$' || true)"
if [[ -n "$unmerged_local" ]]; then
  echo "Unmerged local branches:"
  printf '  %s\n' $unmerged_local
fi

if [[ "$behind" -eq 0 ]]; then
  echo "fork-sync: already up to date with upstream/dev."
  exit 0
fi

echo "==> Incoming upstream/dev commits ($behind):"
git log --oneline -n 30 HEAD..upstream/dev
if [[ "$behind" -gt 30 ]]; then
  echo "  ... ($((behind - 30)) more)"
fi

if [[ "$mode" != "merge" ]]; then
  echo
  echo "Run 'scripts/fork-sync.sh --merge' on a clean tree to merge upstream/dev."
  exit 0
fi

if [[ -n "$(git status --porcelain)" ]]; then
  echo "fork-sync: working tree is not clean; commit or stash changes before --merge." >&2
  exit 1
fi

echo "==> Merging upstream/dev ($upstream_sha) into ${branch:-HEAD}"
if git merge --no-edit -m "chore(sync): merge upstream/dev ($upstream_sha)" upstream/dev; then
  echo "==> Merge succeeded cleanly. Running guardrails (including fork-boundary)..."
  bash "$ROOT/scripts/guardrails.sh"
  echo "fork-sync: merge and guardrails passed. Run 'scripts/clippy.sh' before pushing."
else
  echo
  echo "fork-sync: merge has conflicts. Resolve them per docs/fork-contract.md, then run:"
  echo "  git commit"
  echo "  scripts/guardrails.sh"
  echo "  scripts/clippy.sh"
  exit 1
fi
