#!/usr/bin/env bash
# Single entry point for all cheap guardrails to run before pushing:
#   - scripts/arch-check.sh    (resolved crate layers, cycles, forbid unsafe, App fields, no tests, 800-line cap, wildcards, structure)
#   - scripts/file-size.sh     (repo-wide 800-warn / 1500-error Rust file-size ratchet)
#   - scripts/folder-shape.sh  (prefix families and folder breadth)
#   - scripts/dialog-size.sh   (DialogSize preset enforcement)
#   - scripts/tables-check.sh  (stories order, command order, rune i18n keys/coverage, opencode-plugin-spec --check)
#   - scripts/guardrails/plugin-props.mjs (every ui.<node> prop under plugins/ is one the plugin contract gives it)
#   - scripts/guardrails/engine-revision.mjs (the browser engine's inputs change only together with ENGINE_REVISION)
#   - scripts/guardrails/cursor-allow.mjs (every cursor the app sets is in scripts/cursor-allow.txt with the official line that justifies it)
#   - scripts/guardrails/fork-boundary.mjs (Shuvcode (Rune) canonical identity, compatibility boundaries, and fork deltas)
#
# Slow compile/lint checks remain separate:
#   - scripts/clippy.sh
#   - cargo check -p session-view --examples
#   - pwsh scripts/web.ps1 -Check
#
# Usage: scripts/guardrails.sh
set -euo pipefail
ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"
exec bash "$ROOT/scripts/guardrails/run-js.sh" "$ROOT/scripts/guardrails/run.mjs" "$@"
