# Builds OpenCode (Rune)'s Windows artifacts into -Out:
#
#   OpenCode-Rune-<version>-windows-<arch>.zip        the install folder (opencode-desktop.exe, opencode-desktop.dll,
#                                                     the toasts' icon, the licences), the updater's windows-<arch> target
#   OpenCode-Rune-<version>-windows-<arch>-setup.exe  the setup, with that zip inside
#
#   pwsh apps/installer/scripts/package.ps1 -Out <folder> [-Profile release|dev] [-Arch x64|arm64] [-Cargo <command>] [-Payload <zip>]
#
# -Arch defaults to this machine's. Another arch cross-builds (`--target`), and the app's
# `--version` check is skipped, because this machine can't run it.
#
# -Payload builds only the setup, around a zip built before: the release workflow's
# package-windows job wraps the zip of the install folder that sign-windows signed, so the setup
# installs the same signed files the updater downloads. It writes the setup alone.
#
# Environment, as the macOS build reads it (every artifact of a build gets the same):
#   OPENCODE_RUNE_VERSION, OPENCODE_RUNE_BUILD_CHANNEL   required; `opencode-desktop --version` must report them
#   OPENCODE_RUNE_WINDOW_TITLE                           default "OpenCode (Rune)"
#   OPENCODE_RUNE_BUNDLE_ID                              default "anomaly.opencode-rune"
#   OPENCODE_RUNE_URL_SCHEME                             default "opencode-rune"
#
# -Cargo runs cargo another way, such as a build throttle ("pwsh tools/cargo.ps1 +1.99.1").
#
# .github/workflows/release.yml runs this script in build-windows (x64 and arm64;
# the unsigned setups go to the rolling GitHub pre-release), signs the install folder through
# Azure Artifact Signing, runs it again with -Payload around the signed zip, signs that setup,
# and publishes the zip as target windows-<arch> and the setup as windows-<arch>-setup.
param(
    [Parameter(Mandatory)] [string] $Out,
    [ValidateSet('release', 'dev')] [string] $Profile = 'release',
    [ValidateSet('', 'x64', 'arm64')] [string] $Arch = '',
    [string] $Cargo = 'cargo',
    [string] $Payload = ''
)
$ErrorActionPreference = 'Stop'

$version = $env:OPENCODE_RUNE_VERSION
$channel = $env:OPENCODE_RUNE_BUILD_CHANNEL
if (-not $version -or -not $channel) { throw 'Set OPENCODE_RUNE_VERSION and OPENCODE_RUNE_BUILD_CHANNEL.' }
$root = (Resolve-Path (Join-Path $PSScriptRoot '../../..')).Path
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
$hostArch = if ([Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq 'Arm64') { 'arm64' } else { 'x64' }
if (-not $Arch) { $Arch = $hostArch }
$cross = $Arch -ne $hostArch
$triple = @{ x64 = 'x86_64-pc-windows-msvc'; arm64 = 'aarch64-pc-windows-msvc' }[$Arch]
if ($cross) { $target = Join-Path $target $triple }
$bin = Join-Path $target ($(if ($Profile -eq 'release') { 'release' } else { 'debug' }))
$flags = @(if ($Profile -eq 'release') { '--release' }) + @(if ($cross) { '--target', $triple })
$stem = "Shuvcode-Rune-$version-windows-$Arch"
New-Item -ItemType Directory -Force $Out | Out-Null
$Out = (Resolve-Path $Out).Path

function Invoke-Cargo([string[]] $Arguments) {
    $parts = $Cargo -split ' '
    & $parts[0] @($parts | Select-Object -Skip 1) @Arguments
    if ($LASTEXITCODE -ne 0) { throw "cargo $($Arguments -join ' ') failed" }
}

if ($Payload) {
    $zip = (Resolve-Path $Payload).Path
} else {
    # A release app is the shipped build: its profile-guided optimisation profile comes with it.
    if ($Profile -eq 'release') {
        # A pull request from a branch with "symbols" in its name (or OPENCODE_RUNE_SYMBOLS=1) builds
        # the shipped code with line tables and leaves its PDB in -Out, for profilers that name the
        # source (perf/README.md). Only the debug directory of the exe differs from a build without:
        # release.ps1 asks for the line tables, in a way that keeps every symbol's name.
        $symbols = $env:OPENCODE_RUNE_SYMBOLS -eq '1' -or $env:GITHUB_HEAD_REF -match 'symbols'
        # The shipped app is two files: a thin opencode-desktop.exe (apps/desktop-thin) that loads the
        # app from opencode-desktop.dll beside it (apps/desktop-library: the same code, allocator and
        # profile as the one-file exe). Windows Defender checks a new exe before its first process
        # exists at 30-36 ms per MiB of the exe, and does not check a library that way
        # (apps/desktop-thin/src/main.rs has the numbers). A build that records a profile
        # (release.ps1's `pgo-train/` branches) stays one file, as its training run drives it; so does a
        # pull request from a branch with "one-file" in its name (or OPENCODE_RUNE_ONE_FILE=1): the same
        # code in one exe, to time against.
        $thin = -not ($env:OPENCODE_RUNE_PGO_GENERATE -eq '1' -or $env:GITHUB_HEAD_REF -match '^pgo-train/' -or $env:OPENCODE_RUNE_ONE_FILE -eq '1' -or $env:GITHUB_HEAD_REF -match 'one-file')
        & (Join-Path $root 'apps/desktop/scripts/release.ps1') -Cargo $Cargo -Target $triple -Package $(if ($thin) { 'opencode-desktop-library' } else { 'opencode-desktop' })
        if ($LASTEXITCODE -ne 0) { throw 'the release build failed' }
        if ($thin) { Invoke-Cargo @('build', '--release', '-p', 'opencode-desktop-thin', '--target', $triple) }
        # release.ps1 always names the target, so the app is under target/<triple>/ even when native.
        $appBin = if ($cross) { $bin } else { Join-Path $target "$triple/release" }
    } else {
        $thin = $false
        Invoke-Cargo (@('build', '-p', 'opencode-desktop') + $flags)
        $appBin = $bin
    }

    # The install folder: the app and its toasts' icon, which the setup registers from there
    # (`win_install::register`). The browser engine downloads in the background.
    $stage = Join-Path $Out 'stage'
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
    New-Item -ItemType Directory $stage | Out-Null
    $exe = Join-Path $stage 'opencode-desktop.exe'
    if ($thin) {
        Copy-Item (Join-Path $appBin 'opencode-desktop-thin.exe') $exe
        Copy-Item (Join-Path $appBin 'opencode_desktop_app.dll') (Join-Path $stage 'opencode-desktop.dll')
    } else {
        Copy-Item (Join-Path $appBin 'opencode-desktop.exe') $exe
    }
    if (-not $cross) {
        $reported = (& $exe --version | Out-String).Trim()
        if ($reported -ne "shuvcode-rune $version ($channel)") { throw "opencode-desktop --version reported '$reported', expected 'shuvcode-rune $version ($channel)'" }
    }
    if ($env:OPENCODE_RUNE_BUNDLE_CLI -eq '1') {
        $js = if (Get-Command bun -ErrorAction SilentlyContinue) { 'bun' } elseif (Get-Command node -ErrorAction SilentlyContinue) { 'node' } else { throw 'Need bun or node on PATH to fetch bundled opencode-cli.' }
        & $js (Join-Path $root 'scripts/fetch-cli.mjs') --target "windows-$Arch" --dest $stage --channel $channel
        if ($LASTEXITCODE -ne 0) { throw 'fetching bundled opencode-cli failed' }
    }
    Copy-Item (Join-Path $root 'apps/desktop/assets/icon/art-512.png') (Join-Path $stage 'notification-icon.png')
    Copy-Item (Join-Path $root 'LICENSE') (Join-Path $stage 'LICENSE')
    Copy-Item (Join-Path $root 'THIRD_PARTY_NOTICES.md') (Join-Path $stage 'THIRD_PARTY_NOTICES.md')
    $zip = Join-Path $Out "$stem.zip"
    Remove-Item -Force $zip -ErrorAction SilentlyContinue
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
    Remove-Item -Recurse -Force $stage
    # The PDB keeps the name its image looks for beside itself: the library's, which holds the app.
    if ($Profile -eq 'release' -and $symbols) { Copy-Item (Join-Path $appBin $(if ($thin) { 'opencode_desktop_app.pdb' } else { 'opencode_desktop.pdb' })) $Out }
}

$env:OPENCODE_RUNE_SETUP_PAYLOAD = $zip
try {
    Invoke-Cargo (@('build', '-p', 'opencode-rune-setup') + $flags)
} finally {
    Remove-Item Env:OPENCODE_RUNE_SETUP_PAYLOAD
}
$setup = Join-Path $Out "$stem-setup.exe"
Copy-Item (Join-Path $bin 'opencode-rune-setup.exe') $setup -Force

foreach ($file in @(if (-not $Payload) { $zip }) + $setup) {
    $hash = (Get-FileHash -Algorithm SHA256 $file).Hash.ToLowerInvariant()
    '{0}  {1}  {2:N0} bytes' -f $hash, (Split-Path -Leaf $file), (Get-Item $file).Length
}
