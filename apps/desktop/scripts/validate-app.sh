#!/bin/sh
# Checks a signed OpenCode (Rune).app: its Info.plist, --version output, Developer ID signature,
# signing team, hardened runtime, and entitlements.
#   apps/desktop/scripts/validate-app.sh "<OpenCode (Rune).app>" <version> <channel>
set -eu

[ "$#" -eq 3 ] || { echo "Usage: $0 <App.app> <version> <channel>" >&2; exit 1; }
bundle=$1
version=$2
channel=$3
expected_name=${OPENCODE_RUNE_APP_NAME:-Shuvcode (Rune)}
expected_id=${OPENCODE_RUNE_BUNDLE_ID:-dev.shuv.shuvcode-rune}
team_id=${SEER_TEAM_ID:?Set SEER_TEAM_ID to the Apple Developer signing team}
plist="$bundle/Contents/Info.plist"

fail() {
  echo "OpenCode (Rune).app validation failed: $*" >&2
  exit 1
}

plist_value() {
  /usr/libexec/PlistBuddy -c "Print :$1" "$plist"
}

[ -d "$bundle" ] || fail "$bundle does not exist"
[ "$(basename "$bundle")" = "$expected_name.app" ] || fail "expected $expected_name.app, found $(basename "$bundle")"
[ "$(plist_value CFBundleIdentifier)" = "$expected_id" ] || fail "wrong bundle identifier"
[ "$(plist_value CFBundleExecutable)" = opencode-desktop ] || fail "wrong executable name"
[ "$(plist_value CFBundleName)" = "$expected_name" ] || fail "wrong bundle name"
[ "$(plist_value CFBundleShortVersionString)" = "$version" ] || fail "wrong version"
plist_value NSMicrophoneUsageDescription > /dev/null || fail "no microphone usage description"
[ -x "$bundle/Contents/MacOS/opencode-desktop" ] || fail "packaged executable is missing"
[ "$(lipo -archs "$bundle/Contents/MacOS/opencode-desktop")" = "$(uname -m)" ] || fail "packaged executable does not match the native verify runner"
[ "$("$bundle/Contents/MacOS/opencode-desktop" --version)" = "shuvcode-rune $version ($channel)" ] || fail "opencode-desktop --version is not $version ($channel)"

codesign --verify --deep --strict --verbose=2 "$bundle"
# Every library of the bundle: the hardened runtime loads one only with the app's own team's signature.
for code in "$bundle"/Contents/Frameworks/*.dylib; do
  [ -f "$code" ] || continue
  [ "$(lipo -archs "$code")" = "$(uname -m)" ] || fail "${code##*/} does not match the native verify runner"
  codesign -d --verbose=4 "$code" 2>&1 | grep -Fq "TeamIdentifier=$team_id" || fail "${code##*/} is not signed by the app's team"
done
# Bundled opencode-cli, when this build ships one: arch matches and signed by the app's team.
cli="$bundle/Contents/Resources/opencode-cli"
if [ -e "$cli" ]; then
  [ -x "$cli" ] || fail "bundled opencode-cli is not executable"
  [ "$(lipo -archs "$cli")" = "$(uname -m)" ] || fail "bundled opencode-cli does not match the native verify runner"
  codesign -d --verbose=4 "$cli" 2>&1 | grep -Fq "TeamIdentifier=$team_id" || fail "bundled opencode-cli is not signed by the app's team"
  [ -s "$bundle/Contents/Resources/opencode-cli.version" ] || fail "bundled opencode-cli.version is missing or empty"
fi
# The office viewer's library, when this build ships one: the hardened runtime loads it only with
# the app's own team's signature.
library="$bundle/Contents/Frameworks/libopencode_office.dylib"
if [ -e "$library" ]; then
  [ "$(lipo -archs "$library")" = "$(uname -m)" ] || fail "the office library does not match the native verify runner"
  codesign -d --verbose=4 "$library" 2>&1 | grep -Fq "TeamIdentifier=$team_id" || fail "the office library is not signed by the app's team"
  nm -gU "$library" | grep -q ' _opencode_office_v1$' || fail "the office library does not export opencode_office_v1"
fi
signature=$(codesign -d --verbose=4 "$bundle" 2>&1)
printf '%s\n' "$signature" | grep -Fq "Identifier=$expected_id" || fail "code signature has the wrong identifier"
printf '%s\n' "$signature" | grep -Fq "TeamIdentifier=$team_id" || fail "code signature has the wrong team"
printf '%s\n' "$signature" | grep -Fq 'Authority=Developer ID Application:' || fail "not signed for Developer ID distribution"
printf '%s\n' "$signature" | grep -Eq 'flags=.*runtime' || fail "not signed with the hardened runtime"
entitlements=$(codesign -d --entitlements - "$bundle" 2>/dev/null)
printf '%s\n' "$entitlements" | grep -Fq 'com.apple.security.device.audio-input' || fail "microphone entitlement is missing"
if printf '%s\n' "$entitlements" | grep -Fq 'com.apple.security.app-sandbox'; then
  fail "OpenCode (Rune) must not be sandboxed"
fi

echo "$bundle"
