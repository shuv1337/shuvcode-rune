<h1 align="center">Shuvcode (Rune)</h1>

<p align="center">
  Native Rust desktop &amp; WebGPU client for <a href="https://github.com/shuv1337/shuvcode">Shuvcode</a>, built on <a href="https://github.com/anomalyco/rune">Rune</a>.<br>
  Maintained fork of <a href="https://github.com/anomalyco/opencode-rune">OpenCode (Rune)</a> by Anomaly.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/engine-Rune-242424?style=flat-square&labelColor=161616" alt="Engine: Rune" height="20">
  <img src="https://img.shields.io/badge/backends-Metal%20%C2%B7%20DX12%20%C2%B7%20Vulkan%20%C2%B7%20WebGPU-242424?style=flat-square&labelColor=161616" alt="Backends: Metal, DX12, Vulkan, WebGPU" height="20">
  <img src="https://img.shields.io/badge/electron-0%20MB-242424?style=flat-square&labelColor=161616" alt="Electron: 0 MB" height="20">
  <img src="https://img.shields.io/badge/tests-0-242424?style=flat-square&labelColor=161616" alt="Tests: 0" height="20">
</p>

<p align="center">
  <picture>
    <source srcset="docs/assets/app-hero-dark.png" media="(prefers-color-scheme: dark)">
    <source srcset="docs/assets/app-hero-light.png" media="(prefers-color-scheme: light)">
    <img src="docs/assets/app-hero-light.png" alt="Shuvcode (Rune) session with native GPU generative UI, vector flowchart, and split diff review" width="100%">
  </picture>
</p>

## Connecting to a Shuvcode server

Start your local **Shuvcode** background service (`shuvcode service start`) or point at any running Shuvcode / OpenCode server:

- **Automatic discovery:** reads `~/.local/state/shuvcode/service.json` and `~/.config/shuvcode/service.json` first (including Tailscale / non-loopback addresses and passwords), falling back to OpenCode's `service.json` or `http://127.0.0.1:4919`.
- **Explicit override:** pass `--server <url>` or set `OPENCODE_SERVER_URL`.
- **Never spawns a local server:** `Shuvcode (Rune)` only connects to already-running servers; it never installs or launches a local OpenCode sidecar, and never auto-updates from upstream Anomaly release feeds.

See [`docs/fork-contract.md`](docs/fork-contract.md) for the complete identity, compatibility boundary, and upstream-sync policy.

## Run from source

```sh
cargo run -p opencode-desktop -- --live
```

For fast local iteration (skips debuginfo, Office fonts, and standalone story modules, and enables Luau `--plugin-hot` live reload):

```sh
scripts/dev-fast.sh          # macOS / Linux
pwsh scripts/dev-fast.ps1    # Windows
```

### Other targets

| Target | Command |
| --- | --- |
| **Standalone `session-view` playground** (no server) | `cargo run -p session-view --example read_only` |
| **Local macOS `.app`** (`~/Applications/Shuvcode (Rune).app`) | `apps/desktop/scripts/dev.sh` (or `--release`) |
| **Browser `wasm32`/WebGPU** (`apps/web`) | `pwsh scripts/web.ps1` |

## What's inside

- **Windfoil GPU text.** Draws each glyph's exact coverage directly from its vector outline on the GPU.
- **Zero Chromium at startup.** The built-in browser pane (`⌃⌘B` / `Ctrl+Alt+B`) fetches the shared CEF sidecar lazily on demand.
- **Real native engines.** **Ghostty-VT** (`libghostty-vt`) terminal tabs, virtualized syntax-highlighted code and split diffs (`opencode-code-view`), native GPU ```` ```ui ```` generative UI and vector diagrams (`rune-parts`), Word/Excel/PowerPoint previews via Vello (`office-preview`), local **Hex** voice dictation (`hex-client`), crash-safe SQLite with an mmap draft journal (`opencode-storage`), and 63 locales with RTL.
- **0 ms UI iteration.** Built-in `--!strict` Luau plugins (`plugins/*`) reload live in place with `--plugin-hot` without touching `rustc`.
- **Headless eyes, zero tests.** By upstream policy (see [AGENTS.md](AGENTS.md)), this repo has no automated tests or mocks; verify changes by building, running the app, and running the guardrails.

## Architecture

Organized by **progressive disclosure** across four tiers (full crate inventory and dependency graph in [ARCHITECTURE.md](ARCHITECTURE.md)):

| Tier | Path | Purpose |
| --- | --- | --- |
| **Tier 1** | `plugins/*` | Built-in `--!strict` Luau plugins (panels, dialogs, commands, server sources); hot-reloads without touching Rust |
| **Tier 2** | `features/*` | Decoupled Rust feature crates (`opencode-feature-*`) with zero cross-feature deps and non-generic `&mut dyn *Host` boundaries |
| **Tier 3** | `crates/*` | Shell kernel (`opencode-kernel`), UI foundation (`opencode-ui`), headless runtime (`opencode-runtime`), platform/storage/search/i18n/plugin-runtime libraries, and embeddable crates (`session-view`, `opencode-code-view`, `opencode-kit`, `rune-parts`, `terminal`, `browser-pane`, `office-preview`, `project-icon`, `hex-client`, `otlp-lite`) |
| **Tier 4** | `apps/*` | Host shells and packaging: `apps/desktop` (`opencode-desktop`), `apps/web` (`opencode-web`), `apps/installer` (`opencode-rune-setup`), and `apps/browser-engine` |

Internal Cargo crate names intentionally remain `opencode-*` for clean upstream merges from `anomalyco/opencode-rune`.

## Checks and Upstream Sync

Run the cheap pre-push guardrails (architecture layers, file size, folder shape, dialog sizes, generated tables, cursor allow-list, and the Shuvcode fork-boundary check):

```sh
scripts/guardrails.sh          # macOS / Linux / Git Bash
pwsh scripts/guardrails.ps1    # Windows PowerShell
```

Check or merge incoming commits from `upstream/dev`:

```sh
scripts/fork-sync.sh           # inspect incoming upstream/dev commits
scripts/fork-sync.sh --merge   # fetch and merge upstream/dev into current branch
```

Run `scripts/clippy.sh` separately for workspace-wide `-D warnings` checks across feature sets.

## Roadmap

See [drag-and-drop status and roadmap](docs/drag-and-drop.md) for current desktop gestures and planned improvements to the composer, Files tree, and Review.

## Attribution and License

Shuvcode (Rune) is a fork of [OpenCode (Rune)](https://github.com/anomalyco/opencode-rune), built by Anomaly on [Rune](https://github.com/anomalyco/rune), and is not affiliated with or endorsed by Anomaly. Licensed under the [MIT License](LICENSE); see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
