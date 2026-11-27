#!/usr/bin/env bash
# Builds OpenCode (Rune).app (anomaly.opencode-rune by default) into OUT, ad-hoc signed, for the
# OpenCode (Rune) release and PR workflows. opencode-rune-sign-macos.sh signs it with the
# Developer ID afterwards. Local dev installs (anomaly.opencode-rune.dev) use
# apps/desktop/scripts/bundle.sh and are never touched here.
#
#   apps/desktop/scripts/build-app.sh OUT
#
# Environment:
#   OPENCODE_RUNE_VERSION        (stamped into apps/desktop/Cargo.toml first)
#   OPENCODE_RUNE_BUILD_CHANNEL  (compiled in; `opencode-desktop --version` prints both)
#   OPENCODE_RUNE_APP_NAME       (default: "OpenCode (Rune)")
#   OPENCODE_RUNE_BUNDLE_ID      (default: "anomaly.opencode-rune")
#   OPENCODE_RUNE_ICON           (default: "$ROOT/apps/desktop/assets/icon.icns")
#   OPENCODE_RUNE_URL_SCHEME     (default: "opencode-rune")
#   OPENCODE_RUNE_TARGET         (optional Cargo target triple, e.g. x86_64-apple-darwin)
#   OPENCODE_RUNE_PROFILE        (default: "release"; e.g. "release-thin" for faster dev CI; a pull request
#                                 from a branch with "thin-build" in its name builds release-thin)
#   OPENCODE_RUNE_SYMBOLS        (folder for opencode-desktop.dSYM.zip; default: $RUNNER_TEMP/macos, else OUT)
#   OPENCODE_RUNE_KEEP_SYMBOLS   (1 ships the exe with its symbol names and writes no dSYM)
#   OPENCODE_RUNE_BUNDLE_CLI     (0 skips bundling Contents/Resources/opencode-cli + opencode-cli.version from npm;
#                                 default 1, matching official Desktop)
#   OPENCODE_RUNE_OFFICE_LIBRARY (1 ships the office viewer's engines as Contents/Frameworks/libopencode_office.dylib,
#                                 loaded at the first office file, instead of in the exe; so does a pull request
#                                 from a branch with "office-library" in its name. 0 until the release's signing
#                                 step signs a second Mach-O file.)
#   OPENCODE_RUNE_LAYERS         (1 ships part of the app's code as a library in Contents/Frameworks beside a
#                                 smaller exe (apps/desktop/layers/layers.txt), so the first launch of a new
#                                 build waits for the largest file and not for all the code; so does a pull
#                                 request from a branch with "code-layers" in its name. 0 until the release's
#                                 signing step signs every Contents/Frameworks/*.dylib.)
#   OPENCODE_RUNE_COMPACT_UNWIND (0 keeps a DWARF unwind entry per function; see rustc-compact-unwind.sh)
#   OPENCODE_RUNE_LINK_ORDER     (0 links without apps/desktop/pgo/startup-order.txt; see link-order.sh)
#   OPENCODE_RUNE_PGO_TRAIN      (1 builds the training exe; so does a pull request from a branch
#                                 with "order-train" in its name; see apps/desktop/pgo/README.md)
#   OPENCODE_RUNE_PGO_USE        (0 builds the Apple silicon release without apps/desktop/pgo's profile; so does a
#                                 pull request from a branch with "no-profile" in its name: what the profile is
#                                 worth is that build against the usual one)
set -euo pipefail
OUT=${1:?usage: build-app.sh OUT}
: "${OPENCODE_RUNE_VERSION:?Set OPENCODE_RUNE_VERSION}"
: "${OPENCODE_RUNE_BUILD_CHANNEL:?Set OPENCODE_RUNE_BUILD_CHANNEL}"
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
APP_NAME=${OPENCODE_RUNE_APP_NAME:-Shuvcode (Rune)}
BUNDLE_ID=${OPENCODE_RUNE_BUNDLE_ID:-dev.shuv.shuvcode-rune}
ICON_PATH=${OPENCODE_RUNE_ICON:-$ROOT/apps/desktop/assets/icon.icns}
URL_SCHEME=${OPENCODE_RUNE_URL_SCHEME:-shuvcode-rune}
PROFILE=${OPENCODE_RUNE_PROFILE:-release}
# A pull request from a branch with "thin-build" in its name is built as the dev channel builds
# (release-thin: thin LTO, no profile), so a change can be sized and timed as that channel ships it.
case "${GITHUB_HEAD_REF:-}" in *thin-build*) PROFILE=release-thin ;; esac
# The fonts and sounds are files in the bundle, not bytes in the exe (apps/desktop/build.rs:
# cfg(bundle_assets); src/workspace/system/bundle.rs): macOS reads a new exe whole before its
# first run, and a file in Resources only when the app opens it.
export OPENCODE_RUNE_BUNDLE_ASSETS=1

case "$OPENCODE_RUNE_VERSION" in
  0.0.0-dev-[0-9]*) BUILD=${OPENCODE_RUNE_VERSION#0.0.0-dev-} ;;
  0.0.0-pr-[0-9]*-[0-9]*) BUILD=${OPENCODE_RUNE_VERSION##*-} ;;
  *) BUILD=$(printf '%s\n' "${OPENCODE_RUNE_VERSION%%-*}" | awk -F. '{ print ($1 * 10000) + ($2 * 100) + $3 }') ;;
esac

# The office viewer's engines (Word, Excel, PowerPoint: 24 MiB of code and fonts no launch runs)
# as a library in the bundle. At a bundle's first run macOS reads every file in it, of any kind,
# 8 files at a time, and the launch waits for the largest one (5.5 ms per MiB on an M4, plus 250 ms
# once for code it has never seen): what leaves the exe for a second library is no longer waited
# for (a build never seen: 1,225 -> 1,004 ms thin, 1,057 -> 911 ms fat). One big library in place
# of the exe changes nothing (#958). A file that is not code is read too: it is free only while a
# larger file is read beside it.
OFFICE_LIBRARY=${OPENCODE_RUNE_OFFICE_LIBRARY:-0}
case "${GITHUB_HEAD_REF:-}" in *office-library*) OFFICE_LIBRARY=1 ;; esac
office_args=()
[ "$OFFICE_LIBRARY" != 1 ] || office_args=(-p office-library --features opencode-desktop/office-library)
# Part of the app's code as a library beside a smaller exe (apps/desktop/layers/layers.txt says
# which crates; the office engines are then always the library above). The first launch of a new
# build waits for the largest file, not for all the code. Measured on the Air, dev as the dev
# channel builds it: one exe of 119.6 MiB against exe 41.1 + library 38.0 + office 25.7 MiB:
# first launch of a new install, click -> window on screen 1,093 -> 713 ms; the everyday launch
# 130.4 -> 133.5 ms (exp_728bd8: dyld looks up the 2,800 names the exe takes from the library at
# every launch). More libraries cost more at every launch: 4 of them +6 ms, 8 +17 ms.
LAYERS=${OPENCODE_RUNE_LAYERS:-0}
case "${GITHUB_HEAD_REF:-}" in *code-layers*) LAYERS=1 ;; esac
if [ "$LAYERS" = 1 ]; then
  export OPENCODE_RUNE_LAYERS=1
  OFFICE_LIBRARY=1
  office_args=(-p office-library --features opencode-desktop/office-library)
fi
# A training build records which functions run, how often and in which order (temporal
# instrumentation), for the release profile and the link order. It keeps its names.
TRAIN=${OPENCODE_RUNE_PGO_TRAIN:-0}
case "${GITHUB_HEAD_REF:-}" in *order-train*) TRAIN=1 ;; esac
[ "$TRAIN" != 1 ] || export OPENCODE_RUNE_KEEP_SYMBOLS=1
PGO_USE=${OPENCODE_RUNE_PGO_USE:-1}
case "${GITHUB_HEAD_REF:-}" in *no-profile*) PGO_USE=0 ;; esac

host_triple=$(rustc -vV | sed -n 's/^host: //p')
target_triple=${OPENCODE_RUNE_TARGET:-$host_triple}
cross=0
[ "$target_triple" = "$host_triple" ] || cross=1

# Apple silicon: functions unwind by the linker's compact table instead of a DWARF entry each
# (rustc-compact-unwind.sh): the exe loses its __eh_frame, 9.5 MiB of a thin-LTO build. Measured
# and checked there (`--unwind-check` in smoke-app.sh); an Intel build keeps its entries.
# OPENCODE_RUNE_COMPACT_UNWIND=0 builds as before.
if [ "${OPENCODE_RUNE_COMPACT_UNWIND:-1}" = 1 ] && [ "$target_triple" = aarch64-apple-darwin ]; then
  export RUSTC_WRAPPER="$ROOT/apps/desktop/scripts/rustc-compact-unwind.sh"
fi

if [ -n "${OPENCODE_RUNE_TARGET:-}" ] || [ "$LAYERS" = 1 ] || { [ "$target_triple" = aarch64-apple-darwin ] && [ "$PROFILE" = release ]; }; then
  BIN="${CARGO_TARGET_DIR:-$ROOT/target}/$target_triple/$PROFILE/opencode-desktop"
else
  BIN="${CARGO_TARGET_DIR:-$ROOT/target}/$PROFILE/opencode-desktop"
fi

if [ "${OPENCODE_RUNE_SKIP_CARGO_BUILD:-0}" != "1" ]; then
  # The functions a launch runs go first in the exe, together (link-order.sh): a launch whose exe
  # is not in memory then reads far fewer pages. Cargo takes the linker per target triple.
  if [ "${OPENCODE_RUNE_LINK_ORDER:-1}" = 1 ] && [ "$TRAIN" != 1 ]; then
    export OPENCODE_RUNE_ORDER_LIST="$ROOT/apps/desktop/pgo/startup-order.txt"
    export "CARGO_TARGET_$(printf '%s' "$target_triple" | tr 'a-z-' 'A-Z_')_LINKER=$ROOT/apps/desktop/scripts/link-order.sh"
    rm -f "$(dirname "$BIN")"/deps/opencode_desktop-*.link-order.txt
  fi
  # The code libraries are linked by the same script, with or without the order (link-order.sh).
  [ "$LAYERS" != 1 ] || export "CARGO_TARGET_$(printf '%s' "$target_triple" | tr 'a-z-' 'A-Z_')_LINKER=$ROOT/apps/desktop/scripts/link-order.sh"
  if [ "$LAYERS" = 1 ]; then
    # Rust's standard library goes into the lowest library, with the allocator (layers/base.rs). The
    # toolchain also has it as a library of its own, which a `dylib` crate links by preference, and
    # that one allocates from the system heap whatever the app says. A sysroot of links without
    # that one file leaves rustc the static one. It is given to the target's units only (the build
    # names its target), so build scripts and macros keep the toolchain's own.
    sysroot=$(rustc --print sysroot)
    shadow="${CARGO_TARGET_DIR:-$ROOT/target}/layers-sysroot"
    rm -rf "$shadow"
    mkdir -p "$shadow/lib/rustlib/$target_triple/lib"
    for entry in "$sysroot"/*; do [ "${entry##*/}" = lib ] || ln -s "$entry" "$shadow/"; done
    for entry in "$sysroot"/lib/*; do [ "${entry##*/}" = rustlib ] || ln -s "$entry" "$shadow/lib/"; done
    for entry in "$sysroot"/lib/rustlib/*; do [ "${entry##*/}" = "$target_triple" ] || ln -s "$entry" "$shadow/lib/rustlib/"; done
    for entry in "$sysroot/lib/rustlib/$target_triple"/*; do [ "${entry##*/}" = lib ] || ln -s "$entry" "$shadow/lib/rustlib/$target_triple/"; done
    for entry in "$sysroot/lib/rustlib/$target_triple/lib"/*; do
      case "${entry##*/}" in libstd-*.dylib) ;; *) ln -s "$entry" "$shadow/lib/rustlib/$target_triple/lib/" ;; esac
    done
    flags="CARGO_TARGET_$(printf '%s' "$target_triple" | tr 'a-z-' 'A-Z_')_RUSTFLAGS"
    export "$flags=${!flags:+${!flags} }--sysroot $shadow"
    python3 "$ROOT/apps/desktop/scripts/layers.py" "$target_triple"
  fi
  if [ "$target_triple" = aarch64-apple-darwin ] && [ "$PROFILE" = release ]; then
    # The training build names its target. Keep the same crate identities here and leave
    # host build scripts and proc macros uninstrumented. No training happens on CI.
    (
      cd "$ROOT"
      profile="apps/desktop/pgo/$target_triple.profdata"
      if [ "$TRAIN" = 1 ]; then
        echo "training build: $target_triple, instrumented (set LLVM_PROFILE_FILE when you run it)" >&2
        export CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS="${CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS:+$CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS }-Cprofile-generate -Cllvm-args=-pgo-temporal-instrumentation"
      elif [ "$PGO_USE" != 1 ]; then
        echo "no profile: $target_triple (OPENCODE_RUNE_PGO_USE=0)" >&2
      else
        test -s "$profile" || { echo "Missing release profile: $profile" >&2; exit 1; }
        echo "profile-guided: $target_triple ($(head -n 1 "${profile%.profdata}.txt"))" >&2
        # rustc for a dependency may run in that dependency's directory.
        export CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS="${CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS:+$CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS }-Cprofile-use=$ROOT/$profile -Cllvm-args=-no-pgo-warn-mismatch"
      fi
      cargo build --release --locked --target "$target_triple" -p opencode-desktop ${office_args[@]+"${office_args[@]}"}
    )
  else
    cargo_args=(build --profile "$PROFILE" --locked -p opencode-desktop --manifest-path "$ROOT/Cargo.toml")
    if [ -n "${OPENCODE_RUNE_TARGET:-}" ] || [ "$LAYERS" = 1 ]; then
      cargo_args+=(--target "$target_triple")
    fi
    cargo "${cargo_args[@]}" ${office_args[@]+"${office_args[@]}"}
  fi
fi

# Each code library again, exporting only the names another file takes (layers-exports.py): dyld
# walks every exported name and looks up every taken one at each launch.
if [ "$LAYERS" = 1 ] && [ "${OPENCODE_RUNE_SKIP_CARGO_BUILD:-0}" != "1" ]; then
  python3 "$ROOT/apps/desktop/scripts/layers-exports.py" "$(dirname "$BIN")" "$BIN"
fi

# What the link said about the order (nothing when cargo had no link to do).
cat "$(dirname "$BIN")"/deps/opencode_desktop-*.link-order.txt 2>/dev/null >&2 || true

case "$target_triple" in
  x86_64-apple-darwin) expected_arch=x86_64 ;;
  aarch64-apple-darwin) expected_arch=arm64 ;;
  *) expected_arch= ;;
esac
if [ -n "$expected_arch" ] && command -v lipo >/dev/null 2>&1; then
  actual_arch=$(lipo -archs "$BIN")
  [ "$actual_arch" = "$expected_arch" ] || {
    echo "opencode-desktop arch reported '$actual_arch', expected '$expected_arch'" >&2
    exit 1
  }
fi

if [ "$cross" = "0" ]; then
  EXPECTED_VER="shuvcode-rune $OPENCODE_RUNE_VERSION ($OPENCODE_RUNE_BUILD_CHANNEL)"
  ACTUAL_VER=$("$BIN" --version)
  [ "$ACTUAL_VER" = "$EXPECTED_VER" ] || {
    echo "opencode-desktop --version reported '$ACTUAL_VER', expected '$EXPECTED_VER'" >&2
    exit 1
  }
fi

mkdir -p "$OUT"
APP="$OUT/$APP_NAME.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/opencode-desktop"
# The exe ships without its symbol names. macOS reads every byte of a new executable file before
# its first run (XProtect: 5.6-5.9 ms per MiB on an M4), and the names are 27.5 MiB of a 180 MiB
# thin-LTO exe and 9 MiB of a fat-LTO one. They go into a dSYM first (same UUID as the exe, so
# crash reports, `atos -o`, `sample`, lldb and strobe resolve against it), zipped into SYMBOLS:
# the CI job's artifact folder when it exists, else OUT. Unzip it beside an exe to profile it.
if [ "${OPENCODE_RUNE_KEEP_SYMBOLS:-0}" != "1" ]; then
  SYMBOLS=${OPENCODE_RUNE_SYMBOLS:-${RUNNER_TEMP:+$RUNNER_TEMP/macos}}
  [ -n "$SYMBOLS" ] && [ -d "$SYMBOLS" ] || SYMBOLS=$OUT
  rm -rf "$OUT/opencode-desktop.dSYM" "$SYMBOLS/opencode-desktop.dSYM.zip"
  dsymutil "$BIN" -o "$OUT/opencode-desktop.dSYM" 2>&1 | grep -v 'no debug symbols in executable' >&2 || true
  [ "$(dwarfdump --uuid "$OUT/opencode-desktop.dSYM" | awk '{ print $2 }')" = "$(dwarfdump --uuid "$BIN" | awk '{ print $2 }')" ] || {
    echo "opencode-desktop.dSYM does not carry the exe's UUID" >&2
    exit 1
  }
  /usr/bin/ditto -c -k --keepParent "$OUT/opencode-desktop.dSYM" "$SYMBOLS/opencode-desktop.dSYM.zip"
  rm -rf "$OUT/opencode-desktop.dSYM"
  strip "$APP/Contents/MacOS/opencode-desktop" 2>&1 | grep -v 'will invalidate the code signature' >&2 || true
  [ "$(stat -f %z "$APP/Contents/MacOS/opencode-desktop")" -lt "$(stat -f %z "$BIN")" ] || {
    echo "strip left opencode-desktop as large as it was" >&2
    exit 1
  }
  echo "opencode-desktop: $(stat -f %z "$BIN") -> $(stat -f %z "$APP/Contents/MacOS/opencode-desktop") bytes without symbol names; they are in $SYMBOLS/opencode-desktop.dSYM.zip" >&2
fi
if [ "$OFFICE_LIBRARY" = 1 ]; then
  LIBRARY="$APP/Contents/Frameworks/libopencode_office.dylib"
  mkdir -p "$APP/Contents/Frameworks"
  cp "$(dirname "$BIN")/libopencode_office.dylib" "$LIBRARY"
  # Its one exported name stays (the app looks it up); the local names go, as the exe's do.
  strip -x "$LIBRARY" 2>&1 | grep -v 'will invalidate the code signature' >&2 || true
  nm -gU "$LIBRARY" | grep -q ' _opencode_office_v1$' || {
    echo "libopencode_office.dylib does not export opencode_office_v1" >&2
    exit 1
  }
  codesign --force --sign - "$LIBRARY"
  echo "office viewer: $(stat -f %z "$LIBRARY") bytes in Contents/Frameworks/libopencode_office.dylib, exe $(stat -f %z "$APP/Contents/MacOS/opencode-desktop") bytes" >&2
fi
if [ "$LAYERS" = 1 ]; then
  mkdir -p "$APP/Contents/Frameworks"
  libraries=()
  for built in "$(dirname "$BIN")"/libopencode_layer_*.dylib; do
    [ -f "$built" ] || { echo "no code libraries beside $BIN" >&2; exit 1; }
    libraries+=("${built##*/}")
    cp "$built" "$APP/Contents/Frameworks/"
  done
  # Each file names the libraries it links where cargo built them: in the bundle they are in Frameworks.
  for file in "$APP/Contents/MacOS/opencode-desktop" "${libraries[@]/#/$APP/Contents/Frameworks/}"; do
    for name in "${libraries[@]}"; do
      [ "${file##*/}" != "$name" ] || continue
      old=$(otool -L "$file" | awk -v n="/$name" 'NR > 1 && length($1) >= length(n) && substr($1, length($1) - length(n) + 1) == n && !found { found = $1 } END { if (found) print found }')
      [ -z "$old" ] || install_name_tool -change "$old" "@executable_path/../Frameworks/$name" "$file" 2>&1 | grep -v 'will invalidate the code signature' >&2 || true
    done
  done
  if [ "${OPENCODE_RUNE_KEEP_SYMBOLS:-0}" != "1" ]; then
    rm -rf "$OUT/opencode-desktop-libraries" "$SYMBOLS/opencode-desktop-libraries.dSYM.zip"
    mkdir -p "$OUT/opencode-desktop-libraries"
  fi
  for name in "${libraries[@]}"; do
    library="$APP/Contents/Frameworks/$name"
    install_name_tool -id "@executable_path/../Frameworks/$name" "$library" 2>&1 | grep -v 'will invalidate the code signature' >&2 || true
    if [ "${OPENCODE_RUNE_KEEP_SYMBOLS:-0}" != "1" ]; then
      # The names the other files bind to stay; the local ones go into a dSYM, as the exe's do.
      dsymutil "$(dirname "$BIN")/$name" -o "$OUT/opencode-desktop-libraries/$name.dSYM" 2>&1 | grep -v 'no debug symbols in executable' >&2 || true
      strip -x "$library" 2>&1 | grep -v 'will invalidate the code signature' >&2 || true
    fi
    codesign --force --sign - "$library"
    echo "code library: $(stat -f %z "$library") bytes in Contents/Frameworks/$name" >&2
  done
  if [ "${OPENCODE_RUNE_KEEP_SYMBOLS:-0}" != "1" ]; then
    /usr/bin/ditto -c -k --keepParent "$OUT/opencode-desktop-libraries" "$SYMBOLS/opencode-desktop-libraries.dSYM.zip"
    rm -rf "$OUT/opencode-desktop-libraries"
  fi
  echo "code libraries: ${#libraries[@]} beside an exe of $(stat -f %z "$APP/Contents/MacOS/opencode-desktop") bytes" >&2
fi
cp "$ICON_PATH" "$APP/Contents/Resources/icon.icns"
if [ "${OPENCODE_RUNE_BUNDLE_CLI:-1}" != "0" ]; then
  cli_target=$([ "$target_triple" = "x86_64-apple-darwin" ] && echo darwin-x64 || echo darwin-arm64)
  bash "$ROOT/scripts/guardrails/run-js.sh" "$ROOT/scripts/fetch-cli.mjs" \
    --target "$cli_target" --dest "$APP/Contents/Resources" --channel "$OPENCODE_RUNE_BUILD_CHANNEL"
fi
cp -R "$ROOT/apps/desktop/assets/fonts" "$ROOT/apps/desktop/assets/sounds" "$APP/Contents/Resources/"
# Every language but English: the i18n crate's build wrote them beside the exe (crates/opencode-i18n/build.rs).
rm -rf "$APP/Contents/Resources/i18n"
cp -R "$(dirname "$BIN")/i18n" "$APP/Contents/Resources/i18n"
ls "$APP/Contents/Resources/i18n"/*.spans > /dev/null || { echo "no language files in $(dirname "$BIN")/i18n" >&2; exit 1; }
cp "$ROOT/LICENSE" "$APP/Contents/Resources/LICENSE"
cp "$ROOT/THIRD_PARTY_NOTICES.md" "$APP/Contents/Resources/THIRD_PARTY_NOTICES.md"

xml_escape() {
  printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
}
ESC_NAME=$(xml_escape "$APP_NAME")
ESC_ID=$(xml_escape "$BUNDLE_ID")
ESC_SCHEME=$(xml_escape "$URL_SCHEME")

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>$ESC_NAME</string>
  <key>CFBundleDisplayName</key><string>$ESC_NAME</string>
  <key>CFBundleIdentifier</key><string>$ESC_ID</string>
  <key>CFBundleExecutable</key><string>opencode-desktop</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$OPENCODE_RUNE_VERSION</string>
  <key>CFBundleVersion</key><string>$BUILD</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleURLTypes</key><array><dict><key>CFBundleURLName</key><string>$ESC_ID</string><key>CFBundleURLSchemes</key><array><string>$ESC_SCHEME</string></array></dict></array>
  <key>NSMicrophoneUsageDescription</key><string>Shuvcode (Rune) uses the microphone when you dictate prompts or join voice sessions.</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHumanReadableCopyright</key><string>Copyright © $(date +%Y) Shuvcode (fork of OpenCode)</string>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>NSDesktopFolderUsageDescription</key><string>Shuvcode (Rune) uses files on your Desktop for projects you open and commands you run there.</string>
  <key>NSDocumentsFolderUsageDescription</key><string>Shuvcode (Rune) uses files in your Documents folder for projects you open and commands you run there.</string>
  <key>NSDownloadsFolderUsageDescription</key><string>Shuvcode (Rune) uses files in your Downloads folder for projects you open and commands you run there.</string>
  <key>NSNetworkVolumesUsageDescription</key><string>Shuvcode (Rune) uses files on network volumes for projects you open and commands you run there.</string>
  <key>NSRemovableVolumesUsageDescription</key><string>Shuvcode (Rune) uses files on removable volumes for projects you open and commands you run there.</string>
</dict></plist>
PLIST
plutil -lint "$APP/Contents/Info.plist" >/dev/null
codesign --force --sign - "$APP"
if [ "$LAYERS" = 1 ] && [ "$cross" = "0" ]; then
  # The packaged exe finds its libraries in the bundle (the check above ran it where cargo built it).
  [ "$("$APP/Contents/MacOS/opencode-desktop" --version)" = "shuvcode-rune $OPENCODE_RUNE_VERSION ($OPENCODE_RUNE_BUILD_CHANNEL)" ] || {
    echo "the packaged opencode-desktop does not run with its libraries in Contents/Frameworks" >&2
    exit 1
  }
fi
echo "$APP"
