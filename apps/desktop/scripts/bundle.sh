#!/usr/bin/env bash
# Installs a build as "OpenCode (Rune).app" in ~/Applications and relaunches it without the window
# blinking (the repo's scripts/mac-app). Pass app arguments after the binary path:
#   apps/desktop/scripts/bundle.sh target/release/opencode-desktop --live
#   MAC_APP_FLAGS="--wait-idle 20" apps/desktop/scripts/bundle.sh ...   (unattended rebuilds)
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=${1:-../../target/release/opencode-desktop}
shift || true
test -x "$BIN" || { echo "no binary at $BIN (build with: cargo build --release -p opencode-desktop)" >&2; exit 1; }
BUNDLE_ID=${OPENCODE_RUNE_BUNDLE_ID:-dev.shuv.shuvcode-rune.dev}
# shellcheck disable=SC2086
exec ../../scripts/mac-app --name "Shuvcode (Rune)" --id "$BUNDLE_ID" --exe opencode-desktop \
  --bin "$BIN" --icon assets/icon.icns ${MAC_APP_FLAGS:-} -- "$@"
