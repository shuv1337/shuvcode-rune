#!/usr/bin/env bash
# One-command dev build and install of "OpenCode (Rune).app" into ~/Applications.
#   apps/desktop/scripts/dev.sh              build (dev profile), install, relaunch with --live
#   apps/desktop/scripts/dev.sh --release    same with the release profile
# Skips the relaunch when the binary is unchanged.
set -uo pipefail
export PATH="$HOME/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
APP="$HOME/Applications/Shuvcode (Rune).app"
INSTALLED="$APP/Contents/MacOS/opencode-desktop"
STAMP="$HOME/.cache/shuvcode-rune-build-sha"
PROFILE=debug; CARGO_FLAGS=()
for a in "$@"; do
  case "$a" in
    --release) PROFILE=release; CARGO_FLAGS=(--release) ;;
    *) echo "unknown argument: $a" >&2; exit 2 ;;
  esac
done

say() { echo "$(date '+%F %T') $*"; }
pids() { local p; for p in $(pgrep -x opencode-desktop); do ps -o command= -p "$p" | grep -qF "$INSTALLED" && echo "$p"; done | sort | tr '\n' ' '; }
overlaps() { local p; for p in $1; do case " $2 " in *" $p "*) return 0;; esac; done; return 1; }

start=$SECONDS
bin="$ROOT/target/$PROFILE/opencode-desktop"
( cd "$ROOT" && cargo build -p opencode-desktop ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} ) || { say "build failed"; exit 1; }
test -x "$bin" || { say "no binary at $bin after build"; exit 1; }
sha=$(shasum "$bin" | cut -d" " -f1)
if [ "$(cat "$STAMP" 2>/dev/null)" = "$sha" ] && [ -n "$(pids)" ]; then
  say "binary unchanged, not relaunching ($((SECONDS - start))s)"
  exit 0
fi
before="$(pids)"
"$ROOT/apps/desktop/scripts/bundle.sh" "$bin" --live >/dev/null || { say "bundle.sh failed"; exit 1; }
mkdir -p "$(dirname "$STAMP")"; echo "$sha" > "$STAMP"
after=""
for _ in $(seq 1 50); do
  after="$(pids)"
  if [ -n "$after" ] && ! overlaps "$before" "$after"; then
    say "relaunched: pid ${before:-none} -> $after ($((SECONDS - start))s)"
    echo "done in ${SECONDS}s"
    exit 0
  fi
  sleep 0.2
done
say "FAILED: running pid did not change after relaunch (before: ${before:-none}, now: ${after:-none})"
exit 1
