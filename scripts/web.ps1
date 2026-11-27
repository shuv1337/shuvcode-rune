<#
.SYNOPSIS
  Builds OpenCode (Rune) for the browser (apps/web) and serves it on a local port.

.DESCRIPTION
  Builds apps/web for wasm32-unknown-unknown, runs wasm-bindgen, and puts the page in
  target/web/. Then serves that folder on http://127.0.0.1:<Port>; open it in Chrome or Edge
  (WebGPU) and give it a server URL, or open it with ?server=<url> (and &password=<pw>).

  The OpenCode server must allow the page's origin: a page on http://localhost:<any> or
  http://127.0.0.1:<any> always is. A page anywhere else needs `opencode serve --cors <origin>`.

  Needs: rustup's wasm32-unknown-unknown target, wasm-bindgen-cli at the Cargo.lock version
  (cargo install wasm-bindgen-cli --version <v> --locked), WASI SDK 33 or newer for Luau's C++
  (WASI_SDK_PATH, or -WasiSdk), Bun or Node.js 18+ for the build's steps (scripts/web-*.mjs; a CI
  runner has Node), and Bun for -Check and for the server.

.EXAMPLE
  pwsh scripts/web.ps1                      # release build, serve on 8790
  pwsh scripts/web.ps1 -Dev -Port 8791      # quicker build, bigger and slower page
  pwsh scripts/web.ps1 -NoServe             # build only (CI)
  pwsh scripts/web.ps1 -NoBuild             # serve the last build
  pwsh scripts/web.ps1 -SkipCargo           # package what cargo last built (a build run elsewhere)
  pwsh scripts/web.ps1 -SkipCargo -Names -Out <dir>   # the same, keeping function names for a profile
  pwsh scripts/web.ps1 -Check               # cargo check the app for the web, and that it names no std clock (before a push)
  pwsh scripts/web.ps1 -Check -Cargo <wrapper.ps1>   # the same through a cargo wrapper script
#>
param(
  [int]$Port = 8790,
  [switch]$Dev,
  [switch]$NoBuild,
  [switch]$SkipCargo,
  [switch]$NoServe,
  [switch]$Check,
  # Keep the module's function names (a release build drops them), for a CPU profile that names them.
  [switch]$Names,
  [string]$Cargo = $env:CARGO,
  [string]$WasiSdk = $env:WASI_SDK_PATH,
  [string]$Out
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
if (-not $Out) { $Out = Join-Path $root 'target/web' }
$buildProfile = if ($Dev) { 'debug' } else { 'release' }

if (-not $NoBuild) {
  if (-not $WasiSdk -or -not (Test-Path (Join-Path $WasiSdk 'bin'))) {
    throw 'Set WASI_SDK_PATH (or pass -WasiSdk) to an unpacked WASI SDK 33 or newer: Luau builds for the web with its clang.'
  }
  $exe = if ($IsWindows) { '.exe' } else { '' }
  $env:CC_wasm32_unknown_unknown = Join-Path $WasiSdk "bin/clang$exe"
  $env:CXX_wasm32_unknown_unknown = Join-Path $WasiSdk "bin/clang++$exe"
  $env:AR_wasm32_unknown_unknown = Join-Path $WasiSdk "bin/llvm-ar$exe"
  $env:CXXSTDLIB_wasm32_unknown_unknown = 'c++'
  $env:CFLAGS_wasm32_unknown_unknown = '--target=wasm32-wasip1'
  # The wasm target's C++ alone (cc's per-target variable): the app's build script also compiles
  # Luau for the host, which must not get these. WebAssembly exception handling, as Rune's web build.
  $env:CXXFLAGS_wasm32_unknown_unknown = '--target=wasm32-wasip1 -fexceptions -fwasm-exceptions -mllvm -wasm-use-legacy-eh=false'
  # 2 MiB linear-memory shadow stack (default 1 MiB traps at 167 nested Markdown bullet levels;
  # 2 MiB fits the 255-level nesting bound with +16 Wasm pages and +0 wire bytes).
  if ("$env:CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS" -notmatch '-zstack-size=') {
    $env:CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS = (($env:CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS, '-C link-arg=-zstack-size=2097152') | Where-Object { $_ }) -join ' '
  }
  $env:WASI_SDK_PATH = $WasiSdk
  # A wrapper script runs through pwsh; anything else is cargo itself.
  $runCargo = if (-not $Cargo) { 'cargo' } elseif ($Cargo -like '*.ps1') { 'pwsh' } else { $Cargo }
  $wrap = if ($Cargo -like '*.ps1') { @('-NoProfile', '-File', $Cargo) } else { @() }
  if ($Check) {
    Push-Location $root
    try {
      & $runCargo @wrap check -p opencode-web --lib --target wasm32-unknown-unknown
      if ($LASTEXITCODE) { exit $LASTEXITCODE }
      # std::time::Instant panics in a browser; clippy can't ban it natively, where rune's is std's.
      & bun (Join-Path $PSScriptRoot 'web-clock.ts')
      exit $LASTEXITCODE
    } finally { Pop-Location }
  }
  # The build's own steps (warming table, catalogs, Brotli, page) are plain JavaScript that Bun and
  # Node both run: a CI runner has Node and no Bun. With neither, stop here, before the long build,
  # rather than ship a page without its steps.
  $js = (Get-Command bun -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1).Source
  if (-not $js) {
    $js = (Get-Command node -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1).Source
    if ($js -and [int]((& $js --version) -replace '^v(\d+).*', '$1') -lt 18) { throw "scripts/web.ps1 needs Bun or Node.js 18 or newer for its build steps; this Node is $(& $js --version)." }
  }
  if (-not $js) { throw 'scripts/web.ps1 needs Bun or Node.js 18 or newer on PATH for its build steps (scripts/web-*.mjs); neither is there.' }
  Write-Host "Build steps run with $js"
  $cargoArgs = @('build', '-p', 'opencode-web', '--lib', '--target', 'wasm32-unknown-unknown')
  if (-not $Dev) {
    # Small rather than fast to build: one codegen unit, size-optimised, thin LTO, no symbols.
    $cargoArgs += '--release'
    $env:CARGO_PROFILE_RELEASE_OPT_LEVEL = 's'
    $env:CARGO_PROFILE_RELEASE_LTO = 'thin'
    $env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS = '1'
    $env:CARGO_PROFILE_RELEASE_DEBUG = '0'
    $env:CARGO_PROFILE_RELEASE_STRIP = 'debuginfo'
    $env:CARGO_PROFILE_RELEASE_INCREMENTAL = 'false'
  } else {
    $env:CARGO_PROFILE_DEV_DEBUG = '0'
  }
  if (-not $SkipCargo) {
    Push-Location $root
    try { & $runCargo @wrap @cargoArgs; if ($LASTEXITCODE) { throw "cargo build failed ($LASTEXITCODE)" } } finally { Pop-Location }
  }

  $meta = & cargo metadata --format-version 1 --no-deps --manifest-path (Join-Path $root 'Cargo.toml') | ConvertFrom-Json
  $wasm = Join-Path $meta.target_directory "wasm32-unknown-unknown/$buildProfile/opencode_web.wasm"
  $want = (Select-String (Join-Path $root 'Cargo.lock') -Pattern '^name = "wasm-bindgen"$' -Context 0, 1).Context.PostContext[0] -replace '.*"(.*)".*', '$1'
  $have = (& wasm-bindgen --version) -replace 'wasm-bindgen ', ''
  if ($have -ne $want) { throw "wasm-bindgen $have doesn't match Cargo.lock's ${want}: cargo install wasm-bindgen-cli --version $want --locked" }

  # Build into a staging folder, then swap it in, so a page loading mid-build never mixes builds.
  $next = "$Out.next"
  Remove-Item -Recurse -Force $next -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force $next | Out-Null
  $bindgen = @('--target', 'web', '--out-dir', $next, '--out-name', 'app')
  & wasm-bindgen @bindgen $wasm
  if ($LASTEXITCODE) { throw "wasm-bindgen failed ($LASTEXITCODE)" }
  # The functions the page's warming workers compile ahead of it (`web-warm.mjs`, index.html), found
  # by the module's names, which a release then drops.
  $warm = @((Join-Path $next 'app_bg.wasm'), (Join-Path $root 'apps/web/warm.txt'))
  if ($Dev -or $Names) { $warm += '--keep-names' }
  & $js (Join-Path $PSScriptRoot 'web-warm.mjs') @warm
  if ($LASTEXITCODE) { throw "web-warm failed ($LASTEXITCODE)" }
  # The fonts the page fetches after its first frame (apps/desktop/src/lib.rs's LATE_FONTS), and the terminal's when one attaches (TERMINAL_FONTS).
  New-Item -ItemType Directory -Force (Join-Path $next 'fonts') | Out-Null
  Copy-Item (Join-Path $root 'apps/desktop/assets/fonts/*') (Join-Path $next 'fonts') -Include 'OpenMono-Italic.ttf', 'OpenMono-Icons.ttf', 'CommitMono-*.otf', '*-LICENSE.txt', 'OpenMono-OFL.txt'
  # The spell checker's Hunspell dictionaries (SCOWL's en_US, wooorm/dictionaries `dictionary-en`), fetched when a field first checks.
  New-Item -ItemType Directory -Force (Join-Path $next 'spell') | Out-Null
  Copy-Item (Join-Path $root 'apps/web/spell/*') (Join-Path $next 'spell') -Include '*.aff', '*.dic', '*.LICENSE.txt'
  Copy-Item (Join-Path $root 'LICENSE'), (Join-Path $root 'THIRD_PARTY_NOTICES.md') $next
  # Every locale's messages but English, which the module carries; the page fetches the one it shows.
  & $js (Join-Path $PSScriptRoot 'web-i18n.mjs') (Join-Path $next 'i18n')
  if ($LASTEXITCODE) { throw "web-i18n failed ($LASTEXITCODE)" }
  # A release ships the module and the fonts Brotli-compressed beside themselves, as a static host
  # serves them (`web-serve.ts` sends them to a browser that takes br): the module's 35 MB on the
  # wire becomes about 10, the terminal's 4.8 MB face about 2.
  if (-not $Dev) {
    $packed = @(Join-Path $next 'app_bg.wasm') + @(Get-ChildItem (Join-Path $next 'fonts'), (Join-Path $next 'i18n'), (Join-Path $next 'spell') -File | ForEach-Object FullName)
    & $js -e "const z = require('node:zlib'); const fs = require('node:fs'); for (const f of process.argv.slice(1)) { const b = fs.readFileSync(f); fs.writeFileSync(f + '.br', z.brotliCompressSync(b, { params: { [z.constants.BROTLI_PARAM_QUALITY]: 11, [z.constants.BROTLI_PARAM_SIZE_HINT]: b.length } })); }" @packed
    if ($LASTEXITCODE) { throw "brotli failed ($LASTEXITCODE)" }
  }
  # The sound effects the page plays when they're asked for (apps/desktop/src/workspace/system/sounds.rs).
  Copy-Item -Recurse (Join-Path $root 'apps/desktop/assets/sounds') (Join-Path $next 'sounds')
  # What a phone's Home Screen reads: the web app manifest and the app's icons (`apps/web/pwa`).
  Copy-Item -Recurse (Join-Path $root 'apps/web/pwa/*') $next
  # The page (with the module's size, the build's id and its English messages) and its service worker.
  & $js (Join-Path $PSScriptRoot 'web-page.mjs') $next
  if ($LASTEXITCODE) { throw "web-page failed ($LASTEXITCODE)" }
  Remove-Item -Recurse -Force $Out -ErrorAction SilentlyContinue
  Move-Item $next $Out
  $size = (Get-Item (Join-Path $Out 'app_bg.wasm')).Length
  Write-Host ("Built {0}: app_bg.wasm {1:N0} bytes. Folder: {2}" -f $buildProfile, $size, $Out)
}

if (-not $NoServe) {
  if (-not (Test-Path (Join-Path $Out 'index.html'))) { throw "Nothing built in $Out yet: run without -NoBuild first." }
  Write-Host "Serving $Out on http://127.0.0.1:$Port/ (Ctrl+C stops it)"
  & bun (Join-Path $PSScriptRoot 'web-serve.ts') $Out $Port
}
