#!/usr/bin/env bash
# Build and install Shuvcode (Rune) locally for Linux with a persistent app launcher entry.
#
# Installs:
#   ~/.local/libexec/shuvcode-rune/shuvcode-rune   (atomically replaced binary)
#   ~/.local/bin/shuvcode-rune                     (stable launcher -> local shuvcode service)
#   ~/.local/share/applications/shuvcode-rune.desktop
#   ~/.local/share/icons/hicolor/{512x512,1024x1024}/apps/shuvcode-rune.png
#
# Usage:
#   scripts/install-local.sh              # build with release-quick (default, full features)
#   scripts/install-local.sh --dev-fast   # build with dev-fast (faster compile, no office/stories)
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

profile="release-quick"
cargo_flags=(--profile release-quick)
case "${1:-}" in
  "") ;;
  --dev-fast)
    profile="dev-fast"
    cargo_flags=(--profile dev-fast --no-default-features --features knobs)
    ;;
  *)
    echo "Usage: scripts/install-local.sh [--dev-fast]" >&2
    exit 2
    ;;
esac

echo "==> Building opencode-desktop ($profile)"
cargo build -p opencode-desktop "${cargo_flags[@]}"

src_bin="$ROOT/target/$profile/opencode-desktop"
if [[ ! -x "$src_bin" ]]; then
  echo "install-local: built binary not found at $src_bin" >&2
  exit 1
fi

version="$("$src_bin" --version)"
if [[ "$version" != shuvcode-rune\ * ]]; then
  echo "install-local: refusing to install non-fork binary ($src_bin): $version" >&2
  exit 1
fi
echo "==> Verified binary: $version"

libexec_dir="$HOME/.local/libexec/shuvcode-rune"
bin_dir="$HOME/.local/bin"
apps_dir="$HOME/.local/share/applications"
icon_512_dir="$HOME/.local/share/icons/hicolor/512x512/apps"
icon_1024_dir="$HOME/.local/share/icons/hicolor/1024x1024/apps"

mkdir -p "$libexec_dir" "$bin_dir" "$apps_dir" "$icon_512_dir" "$icon_1024_dir"

# Atomic install so replacing a running binary never hits ETXTBSY.
tmp_bin="$libexec_dir/.shuvcode-rune.new.$$"
install -m 755 "$src_bin" "$tmp_bin"
mv -f "$tmp_bin" "$libexec_dir/shuvcode-rune"

cat > "$bin_dir/shuvcode-rune" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

binary="$HOME/.local/libexec/shuvcode-rune/shuvcode-rune"
registration="${XDG_STATE_HOME:-$HOME/.local/state}/shuvcode/service.json"

if [[ ! -x "$binary" ]]; then
    printf 'Shuvcode (Rune) is not installed: %s\n' "$binary" >&2
    exit 1
fi

version="$("$binary" --version 2>/dev/null || true)"
if [[ "$version" != shuvcode-rune\ * ]]; then
    printf 'Refusing to launch non-fork binary (%s): %s\n' "$binary" "${version:-unknown}" >&2
    exit 1
fi

if [[ "${1:-}" == "--version" ]]; then
    printf '%s\n' "$version"
    exit 0
fi

if ! url=$(jq -er '.url | select(type == "string" and length > 0)' "$registration") ||
    ! OPENCODE_SERVER_PASSWORD=$(jq -er '.password | select(type == "string" and length > 0)' "$registration"); then
    printf 'Could not read the running Shuvcode service registration: %s\n' "$registration" >&2
    printf 'Start your Shuvcode service first (shuvcode service start); Shuvcode (Rune) never starts a server.\n' >&2
    exit 1
fi
export OPENCODE_SERVER_PASSWORD

exec "$binary" --live --server "$url" "$@"
EOF
chmod 755 "$bin_dir/shuvcode-rune"

install -m 644 "$ROOT/apps/desktop/assets/icon/art-512.png" "$icon_512_dir/shuvcode-rune.png"
install -m 644 "$ROOT/apps/desktop/assets/icon/icon-1024.png" "$icon_1024_dir/shuvcode-rune.png"

cat > "$apps_dir/shuvcode-rune.desktop" <<EOF
[Desktop Entry]
Version=1.0
Type=Application
Name=Shuvcode (Rune)
GenericName=AI Coding Client
Comment=Native Rust desktop client for Shuvcode
Exec=$bin_dir/shuvcode-rune %u
TryExec=$bin_dir/shuvcode-rune
Icon=shuvcode-rune
Terminal=false
Categories=Development;
Keywords=Shuvcode;Rune;AI;coding;agent;
StartupWMClass=shuvcode-rune
MimeType=x-scheme-handler/shuvcode-rune;
EOF
chmod 644 "$apps_dir/shuvcode-rune.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database -q "$apps_dir" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -q -t -f "$HOME/.local/share/icons/hicolor" || true
fi

echo "==> Installed $version"
echo "    Binary:   $libexec_dir/shuvcode-rune"
echo "    Launcher: $bin_dir/shuvcode-rune"
echo "    Desktop:  $apps_dir/shuvcode-rune.desktop"
