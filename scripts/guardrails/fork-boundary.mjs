// Executable fork-boundary check for shuv1337/shuvcode-rune (fork of anomalyco/opencode-rune).
// Fails if canonical Shuvcode (Rune) identity drifts, retired upstream branding reappears in
// canonical surfaces, deliberate fork deltas are undone by an upstream merge, or preserved
// compatibility/provenance boundaries break.
//
// Runs on Bun or Node >= 18. Wired into scripts/guardrails/run.mjs.
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(fileURLToPath(new URL("../..", import.meta.url)));
const errors = [];

function read(relPath) {
  const full = join(root, relPath);
  if (!existsSync(full)) {
    errors.push(`missing required file: ${relPath}`);
    return null;
  }
  return readFileSync(full, "utf8");
}

function mustContain(relPath, needle, reason) {
  const text = read(relPath);
  if (text === null) return;
  if (!text.includes(needle)) {
    errors.push(`${relPath}: missing ${JSON.stringify(needle)} (${reason})`);
  }
}

function mustNotContain(relPath, needle, reason) {
  const text = read(relPath);
  if (text === null) return;
  if (text.includes(needle)) {
    errors.push(`${relPath}: contains retired ${JSON.stringify(needle)} (${reason})`);
  }
}

// 1. Provenance & fork contract docs
mustContain("docs/fork-contract.md", "shuv1337/shuvcode-rune", "fork contract repository");
mustContain("docs/fork-contract.md", "anomalyco/opencode-rune", "upstream provenance");
mustContain("LICENSE", "MIT License", "MIT license preserved");
if (!existsSync(join(root, "THIRD_PARTY_NOTICES.md"))) {
  errors.push("missing THIRD_PARTY_NOTICES.md");
}
mustContain("README.md", "Shuvcode (Rune)", "canonical README title");
mustContain("README.md", "anomalyco/opencode-rune", "upstream attribution in README");
mustNotContain("README.md", "anoma.ly/release/opencode/desktop-rune", "upstream release download links");

// 2. Canonical product identity + compatibility scheme (crates/win-install/src/product.rs)
mustContain("crates/win-install/src/product.rs", 'PUBLISHER: &str = "Shuvcode"', "canonical Windows publisher");
mustContain("crates/win-install/src/product.rs", 'HOMEPAGE: &str = "https://github.com/shuv1337/shuvcode-rune"', "canonical homepage");
mustContain("crates/win-install/src/product.rs", 'FAMILY: &str = "dev.shuv.shuvcode-rune"', "canonical bundle/AUMID family");
mustContain("crates/win-install/src/product.rs", '"Shuvcode (Rune)"', "canonical default display name");
mustContain("crates/win-install/src/product.rs", 'own("shuvcode-rune", \'-\')', "canonical URL scheme");
mustContain("crates/win-install/src/product.rs", 'OPENCODE_SCHEME: &str = "opencode"', "preserved compatibility scheme");
mustNotContain("crates/win-install/src/product.rs", 'FAMILY: &str = "anomaly.opencode-rune"', "retired upstream family");
mustNotContain("crates/win-install/src/product.rs", 'None => "OpenCode (Rune)"', "retired upstream default name");

// 3. Isolated profile directories + shared browser engine compatibility (dirs.rs & install.rs)
mustContain("apps/desktop/src/workspace/system/dirs.rs", 'APP_ID: &str = "shuvcode-rune"', "canonical data dir id");
mustContain("apps/desktop/src/workspace/system/dirs.rs", 'DEBUG_APP_ID: &str = "shuvcode-rune.debug"', "canonical debug data dir id");
mustContain("apps/desktop/src/workspace/system/dirs.rs", '"dev.shuv.shuvcode-rune"', "canonical macOS updates cache bundle");
mustContain("apps/desktop/src/workspace/system/dirs.rs", '"co.anomaly.opencode"', "preserved shared browser engine dir");
mustContain("apps/desktop/src/workspace/system/dirs.rs", '"ai.opencode.desktop"', "preserved official desktop seed dirs");
mustNotContain("apps/desktop/src/workspace/system/dirs.rs", 'APP_ID: &str = "rune-opencode"', "retired upstream data dir id");
mustNotContain("apps/desktop/src/workspace/system/dirs.rs", '"anomaly.opencode-rune"', "retired upstream bundle id");
mustContain("apps/desktop/src/workspace/system/install.rs", 'strip_prefix("dev.shuv.shuvcode-rune.")', "canonical uninstall folder lookup");
mustNotContain("apps/desktop/src/workspace/system/install.rs", 'strip_prefix("anomaly.opencode-rune.")', "retired upstream uninstall prefix");

// 4. Desktop boot identity (boot.rs, telemetry.rs, web.rs)
mustContain("apps/desktop/src/workspace/lifecycle/launch/boot.rs", 'shuvcode-rune {} ({})', "canonical --version output");
mustContain("apps/desktop/src/workspace/lifecycle/launch/boot.rs", 'rune::set_app_id("shuvcode-rune")', "canonical Linux app_id");
mustContain("apps/desktop/src/workspace/lifecycle/launch/boot.rs", 'let fallback = "Shuvcode (Rune)";', "canonical wasm window title");
mustContain("crates/opencode-platform/src/telemetry.rs", 'otlp_lite::start("shuvcode-rune"', "canonical telemetry service name");
mustContain("crates/opencode-storage/src/web.rs", 'const DB: &str = "shuvcode-rune-storage";', "canonical web IndexedDB name");

// 5. Web app identity + compatibility auth username
mustContain("apps/web/index.html", "<title>Shuvcode (Rune)</title>", "canonical web title");
mustContain("apps/web/index.html", 'value="opencode"', "preserved HTTP Basic auth username");
mustContain("apps/web/index.html", "opencode.global.dat:language", "preserved locale localStorage key");
mustContain("apps/web/pwa/manifest.webmanifest", '"name": "Shuvcode (Rune)"', "canonical PWA name");
mustContain("apps/web/pwa/manifest.webmanifest", '"short_name": "Shuvcode"', "canonical PWA short_name");

// 6. Packaging scripts
mustContain("apps/desktop/scripts/package-linux.sh", 'expected="shuvcode-rune $version', "canonical Linux --version check");
mustContain("apps/desktop/scripts/package-linux.sh", "Name=Shuvcode (Rune)", "canonical Linux .desktop name");
mustContain("apps/desktop/scripts/package-linux.sh", "Package: shuvcode-rune", "canonical deb package name");
mustContain("apps/desktop/scripts/package-linux.sh", "OPENCODE_RUNE_BUNDLE_CLI:-0", "fork delta: do not bundle opencode-cli by default");
mustContain("apps/desktop/scripts/build-app.sh", 'APP_NAME=${OPENCODE_RUNE_APP_NAME:-Shuvcode (Rune)}', "canonical macOS app name");
mustContain("apps/desktop/scripts/build-app.sh", 'BUNDLE_ID=${OPENCODE_RUNE_BUNDLE_ID:-dev.shuv.shuvcode-rune}', "canonical macOS bundle id");
mustContain("apps/installer/scripts/package.ps1", '$stem = "Shuvcode-Rune-$version-windows-$Arch"', "canonical Windows artifact stem");

// 7. Deliberate fork deltas (sidecar disabled, shuvcode discovery, port 4919, upstream updater disabled)
const sidecar = read("apps/desktop/src/workspace/servers/sidecar/mod.rs");
if (sidecar !== null && !/pub fn managed\(\) -> bool \{\s*(?:\/\/[^\n]*\n\s*)*false\s*\}/.test(sidecar)) {
  errors.push("apps/desktop/src/workspace/servers/sidecar/mod.rs: sidecar::managed() must unconditionally return false");
}
const localService = read("crates/opencode-kit/src/local_service.rs");
if (localService !== null) {
  const shuvIdx = localService.indexOf('.join("shuvcode")');
  const ocIdx = localService.indexOf('.join("opencode")');
  if (shuvIdx === -1 || ocIdx === -1 || shuvIdx > ocIdx) {
    errors.push("crates/opencode-kit/src/local_service.rs: must check shuvcode/service.json before opencode/service.json");
  }
}
mustContain("crates/opencode-kit/src/client.rs", 'DEFAULT_SERVER: &str = "http://127.0.0.1:4919"', "fork delta: Shuvcode default port 0x1337");
mustContain("features/egg/src/view.rs", 'shuv1337 fork: "shuvcode" wordmark', "canonical shuvcode new-session wordmark");
mustContain("features/egg/src/draw.rs", 'shuv1337 fork: "shuvcode" logo blocks', "canonical shuvcode animated wordmark");
mustContain("features/egg/src/pins/mod.rs", 'shuv1337 fork: the "shuvcode" wordmark', "canonical shuvcode 3D pin board");
mustContain("features/settings/src/pages/about.rs", "['s', 'h', 'u', 'v', 'c', 'o', 'd', 'e']", "canonical shuvcode About wordmark");
const updaterPlatform = read("features/updater/src/platform.rs");
if (updaterPlatform !== null && !/fn fork_updater_enabled\(\) -> bool \{\s*(?:\/\/[^\n]*\n\s*)*false\s*\}/.test(updaterPlatform)) {
  errors.push("features/updater/src/platform.rs: fork_updater_enabled() must return false so fork never auto-updates from anoma.ly");
}

if (errors.length > 0) {
  for (const err of errors) console.error(`  FAIL   ${err}`);
  console.error(`fork-boundary: ${errors.length} error(s)`);
  process.exit(1);
}

console.log("fork-boundary: ok (canonical Shuvcode (Rune) identity, compatibility boundaries, and fork deltas intact)");
