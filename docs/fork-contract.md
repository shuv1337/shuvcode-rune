# Shuvcode (Rune) Fork Contract

This document is the authoritative identity, compatibility, and upstream-sync
contract for **Shuvcode (Rune)** (`shuv1337/shuvcode-rune`), a maintained fork
of **OpenCode (Rune)** (`anomalyco/opencode-rune`).

`scripts/guardrails/fork-boundary.mjs` (run by `scripts/guardrails.sh` and
`pwsh scripts/guardrails.ps1`) enforces this contract automatically on every
change and upstream merge.

## Provenance and Remotes

| Field | Value |
| --- | --- |
| Upstream repository | `https://github.com/anomalyco/opencode-rune` (`upstream/dev`) |
| Upstream base at fork | `99459a2c` (`Upstream ledger round 133: official to 8ec08f4913 (#1339)`) |
| Fork repository | `https://github.com/shuv1337/shuvcode-rune` (`origin/dev`) |
| Default branch | `dev` |
| License | MIT (`LICENSE`, `THIRD_PARTY_NOTICES.md`) |
| Upstream sync policy | Regular merges from `upstream/dev` into `dev` |

Commit history is preserved. Upstream attribution to Anomaly, OpenCode (Rune),
OpenCode, and Rune (`anomalyco/rune`) is retained in `README.md`, `LICENSE`,
`THIRD_PARTY_NOTICES.md`, and source comments.

## Identity and Compatibility Classification

Every occurrence of the upstream identity falls into one of four classes:

### 1. Canonical (Renamed)

| Surface | Upstream | Shuvcode (Rune) |
| --- | --- | --- |
| Product / display name | `OpenCode (Rune)` | `Shuvcode (Rune)` (`Shuvcode (Rune) Stable`, `Shuvcode (Rune) Beta`) |
| Install folder (Windows) | `OpenCode Rune` | `Shuvcode Rune` |
| Slug / Linux executable / `.desktop` / `WM_CLASS` / deb & rpm package | `opencode-rune` | `shuvcode-rune` |
| `--version` output | `opencode-rune <ver> (<ch>)` | `shuvcode-rune <ver> (<ch>)` |
| Bundle ID / AppUserModelID family | `anomaly.opencode-rune` | `dev.shuv.shuvcode-rune` (`.dev`, `.pr.<N>`, `.stable`, `.beta`) |
| URL scheme | `opencode-rune` | `shuvcode-rune` (`shuvcode-rune-dev`, `shuvcode-rune-{stable,beta}`) |
| App data / config / cache / log dir ID | `rune-opencode` / `opencode-rune` | `shuvcode-rune` (`shuvcode-rune.debug`, `shuvcode-rune.pr-<N>`) |
| Web IndexedDB name | `opencode-rune-storage` | `shuvcode-rune-storage` |
| Web title / PWA manifest | `OpenCode (Rune)` / `OpenCode` | `Shuvcode (Rune)` / `Shuvcode` |
| Telemetry service name | `opencode-rune` | `shuvcode-rune` |
| Publisher / Maintainer | `OpenCode` / `Anomaly <support@opencode.ai>` | `Shuvcode` / `shuv <shuv@shuv.dev>` |
| Homepage / Repository | `https://opencode.ai` / `anomalyco/opencode-rune` | `https://github.com/shuv1337/shuvcode-rune` |
| Default fallback server URL | `http://127.0.0.1:4096` | `http://127.0.0.1:4919` (`0x1337`, matching `shuvcode`) |

### 2. Compatibility (Preserved Byte-for-Byte)

These identifiers are intentionally preserved at external and internal
boundaries so `shuvcode-rune` interoperates with `shuvcode` / OpenCode servers
and merges cleanly with `upstream/dev`:

- **Internal Rust crate and Cargo package names**: `opencode-desktop`,
  `opencode-desktop-library`, `opencode-desktop-thin`, `opencode-rune-setup`,
  `opencode-web`, `opencode-feature-*`, `opencode-kernel`, `opencode-kit`,
  `opencode-ui`, `opencode-storage`, `opencode-platform`, `opencode-runtime`,
  `opencode-search`, `opencode-servers`, `opencode-code-view`, `opencode-i18n`,
  `opencode-plugin-runtime`, `opencode-plugin-spec`, `session-view`. (Matches
  `shuv1337/shuvcode`, which keeps internal `@opencode/*` package scopes.)
- **Environment variables**: `OPENCODE_SERVER_URL`, `OPENCODE_SERVER_PASSWORD`,
  `OPENCODE_CONFIG_DIR`, `OPENCODE_DESKTOP_SERVER_CHANNEL`, `OPENCODE_RUNE_*`,
  `RUNE_DIRS_ROOT`.
- **Wire protocol and auth**: HTTP API routes (`/api/info`, `/api/event`, ...),
  Basic auth username `opencode`, `service.json` / `service-local.json` schema,
  fallback discovery of `<state>/opencode/service.json` and
  `<config>/opencode/service.json` *after* `shuvcode/`.
- **Storage formats**: SQLite filename `opencode.sqlite`, state namespaces
  (`opencode.settings`, `opencode.global.dat`, `opencode.window.<id>.dat`,
  `plugin.*`, `rune.*`), `localStorage` keys (`opencode.global.dat:language`),
  and read-only seeding from Official Desktop folders (`ai.opencode.desktop*`).
- **Shared browser engine**: `co.anomaly.opencode` shared folder and upstream
  browser-engine download index so the lazy CEF browser pane continues to work
  without self-hosting Chromium/CEF builds.
- **Scheme compatibility**: `OPENCODE_SCHEME = "opencode"` stays available
  alongside `shuvcode-rune://`.
- **Upstream Rune pin**: All `anomalyco/rune` dependencies and patches in root
  `Cargo.toml` stay on the same pinned rev (`scripts/arch-check.sh`).
- **Luau plugin contract & guardrails**: `crates/opencode-plugin-spec/contract.json`
  and `scripts/cursor-allow.txt`.

### 3. Provenance (Preserved)

- `LICENSE` and `THIRD_PARTY_NOTICES.md`
- Upstream links to `anomalyco/opencode-rune`, `anomalyco/opencode`, and
  `anomalyco/rune`
- Official OpenCode reference comments (`packages/app/...`, `packages/desktop/...`)
- `docs/upstream-sync/` ledger

### 4. Incidental (Tracked Upstream)

- **63-locale i18n catalogs** (`crates/opencode-i18n/locales/rune/*.json`):
  kept tracking upstream verbatim to avoid 63-file merge conflicts on every
  upstream sync.
- **Stories and fixtures** (`apps/desktop/src/stories/`, `--fixture`): kept
  tracking upstream.
- **Upstream `.github/workflows/`**: upstream release and PR workflows target
  Anomaly's runners and AWS OIDC signing secrets (`anomalyco/opencode-rune`) and
  are outside fork publishing.

## Deliberate Fork Deltas

Keep the fork overlay minimal and clearly marked with `// shuv1337 fork:`
comments so every `upstream/dev` merge is predictable:

1. **Never install or start a local OpenCode server** (`ae901e7d`):
   - `apps/desktop/src/workspace/servers/sidecar/mod.rs`: `sidecar::managed()`
     always returns `false`.
   - `apps/desktop/scripts/package-linux.sh` and
     `apps/installer/scripts/package.ps1`: `OPENCODE_RUNE_BUNDLE_CLI` defaults
     to `0`.
2. **Discover local `shuvcode` service first, including Tailscale addresses**
   (`bc4aee54`):
   - `crates/opencode-kit/src/local_service.rs`: reads
     `<state>/shuvcode/service.json` and `<config>/shuvcode/service.json` ahead
     of `opencode/`, accepts non-loopback hosts named in `service.json`, and
     matches passwords for non-loopback `service.json` bases.
   - `crates/opencode-kit/src/client.rs`: `DEFAULT_SERVER` is
     `http://127.0.0.1:4919` (`0x1337`).
3. **Never auto-update from Anomaly's release feed**:
   - `features/updater/src/platform.rs`: `fork_updater_enabled()` returns
     `false` so a `shuvcode-rune` build never checks `https://anoma.ly/release`
     or overwrites itself with upstream `OpenCode (Rune)`.
4. **Canonical Shuvcode (Rune) identity & isolated profile directories**:
   - Window title, bundle ID, URL scheme, Linux `.desktop`/`WM_CLASS`/package,
     and `shuvcode-rune` app data/config/cache/log directories so
     `Shuvcode (Rune)` can coexist cleanly beside `OpenCode (Rune)`.

## Regular Upstream Sync Policy

Keep `dev` in sync with `upstream/dev` regularly using plain `git` and
`scripts/fork-sync.sh`:

1. Inspect incoming upstream commits:
   ```sh
   scripts/fork-sync.sh
   ```
2. On a clean tree (or dedicated `sync-upstream-<date>` branch), merge
   `upstream/dev`:
   ```sh
   scripts/fork-sync.sh --merge
   ```
3. Resolve any conflicts by preserving the four deliberate deltas and the
   canonical identity table above while accepting upstream changes everywhere
   else.
4. Run validation gates:
   ```sh
   scripts/guardrails.sh
   scripts/clippy.sh
   ```
   `scripts/guardrails.sh` runs `fork-boundary`, which fails immediately if an
   upstream merge reintroduces retired branding, re-enables the local sidecar,
   re-enables the Anomaly auto-updater, or breaks compatibility boundaries.
5. Push `dev` (or open a sync PR targeting `dev` on `origin`, never `upstream`).
