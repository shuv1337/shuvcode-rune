#!/usr/bin/env bash
# opencode-rune-sign-macos.sh <unsigned app zip> <out dir> <arch>: signs "OpenCode (Rune).app"
# inside-out (each Contents/Frameworks/*.dylib, then the app) with Anomaly's Developer ID identity
# (hardened runtime, secure timestamp, microphone entitlement on the app), notarizes and staples
# it, then writes:
#   OpenCode-Rune-<version>-macos-<arch>.zip  (root "OpenCode (Rune).app", for the auto-updater)
#   OpenCode-Rune-<version>-macos-<arch>.dmg  (signed, notarized, stapled, with /Applications link)
#
# Optional overrides for side-by-side PR builds:
#   OPENCODE_RUNE_APP_NAME    (default: "OpenCode (Rune)")
#   OPENCODE_RUNE_BUNDLE_ID   (default: "anomaly.opencode-rune")
#   OPENCODE_RUNE_FILE_STEM   (default: "OpenCode-Rune-$OPENCODE_RUNE_VERSION")
. "$(dirname "$0")/lib.sh"

input=$1
out=$2
release=$(cd "$(dirname "$0")" && pwd)
app_name=${OPENCODE_RUNE_APP_NAME:-OpenCode (Rune)}
bundle_id=${OPENCODE_RUNE_BUNDLE_ID:-anomaly.opencode-rune}
stem=${OPENCODE_RUNE_FILE_STEM:-OpenCode-Rune-$OPENCODE_RUNE_VERSION}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$out"
arch=${3:-arm64}
zip="$out/$stem-macos-$arch.zip"
dmg="$out/$stem-macos-$arch.dmg"

/usr/bin/ditto -x -k "$input" "$work/unpacked"
app="$work/unpacked/$app_name.app"
if [ ! -d "$app" ] || [ "$(find "$work/unpacked" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')" != 1 ]; then
  die "The macOS build archive must hold exactly $app_name.app."
fi
[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app/Contents/Info.plist")" = "$bundle_id" ] ||
  die "$app_name.app has the wrong bundle identifier."
[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")" = "$OPENCODE_RUNE_VERSION" ] ||
  die "$app_name.app is not version $OPENCODE_RUNE_VERSION."

# The bundle's code: the app's executable, plus any dylibs directly in Contents/Frameworks (the
# executable split into libraries). Anything else that is Mach-O fails the release.
found=$(cd "$app" && find . -type f -exec sh -c 'file -b "$1" | grep -q Mach-O && printf "%s\n" "${1#./}"' _ {} \; | LC_ALL=C sort)
grep -Fxq Contents/MacOS/opencode-desktop <<< "$found" || die "$app_name.app has no Contents/MacOS/opencode-desktop."
dylibs=()
bundled_cli=""
unexpected=""
while IFS= read -r path; do
  case "$path" in
    Contents/MacOS/opencode-desktop) ;;
    Contents/Resources/opencode-cli) bundled_cli="$path" ;;
    Contents/Frameworks/*.dylib)
      if [ "${path#Contents/Frameworks/}" = "${path##*/}" ]; then dylibs+=("$path"); else unexpected+=" $path"; fi
      ;;
    *) unexpected+=" $path" ;;
  esac
done <<< "$found"
[ -z "$unexpected" ] || die "Unexpected Mach-O code in $app_name.app:$unexpected"

sign() {
  codesign --force --timestamp --options runtime --keychain "$SEER_KEYCHAIN" --sign "$SEER_SIGNING_IDENTITY" "$@"
}
notarize() {
  xcrun notarytool submit "$1" --key "$SEER_NOTARY_KEY" --key-id "$SEER_NOTARY_KEY_ID" --issuer "$SEER_NOTARY_ISSUER" --wait --timeout 45m
}

# Inside out: every dylib first (no entitlements: they belong to the app), then the bundled Bun CLI
# (matching official desktop-cli.plist: allow-jit + disable-library-validation for bun-pty), then
# the app.
for lib in ${dylibs[@]+"${dylibs[@]}"}; do
  sign "$app/$lib"
done
if [ -n "$bundled_cli" ]; then
  sign --entitlements "$release/opencode-rune-cli.entitlements" "$app/$bundled_cli"
fi
sign --entitlements "$release/opencode-rune.entitlements" "$app"
codesign --verify --deep --strict --verbose=2 "$app"

/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$app" "$work/notarize.zip"
notarize "$work/notarize.zip"
xcrun stapler staple "$app"
xcrun stapler validate "$app"

rm -f "$zip" "$dmg"
/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$app" "$zip"

mkdir "$work/staging"
/usr/bin/ditto "$app" "$work/staging/$app_name.app"
ln -s /Applications "$work/staging/Applications"
hdiutil create -quiet -fs APFS -format ULFO -volname "$app_name" -srcfolder "$work/staging" "$dmg"
codesign --force --timestamp --keychain "$SEER_KEYCHAIN" --sign "$SEER_SIGNING_IDENTITY" "$dmg"
notarize "$dmg"
xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"
ls -l "$out"
