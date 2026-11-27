//! Where this app keeps its files, resolved once from the environment through `rune::dirs`.
//!
//! | What | macOS | Windows |
//! | --- | --- | --- |
//! | The app's database (`opencode.sqlite`: settings, windows, tabs, drafts, prompt history) | `~/Library/Application Support/rune-opencode` | `%APPDATA%\rune-opencode` |
//! | Caches (Office fonts, updates) | `~/Library/Caches/rune-opencode` | `%LOCALAPPDATA%\rune-opencode\Cache` |
//! | Motion log, frame diagnostics (`perf/`) | `~/Library/Logs/opencode-rune` | `%LOCALAPPDATA%\rune-opencode\Logs` |
//! | Update downloads and records | `~/Library/Caches/<team bundle id>/Updates` | `%LOCALAPPDATA%\rune-opencode\Cache\Updates` |
//! | Browser engine: installed versions, browser profiles (shared with every app with a browser pane) | `~/Library/Application Support/co.anomaly.opencode` | `%LOCALAPPDATA%\co.anomaly.opencode` |
//!
//! Linux follows XDG. The folder names differ from OpenCode Desktop's (`ai.opencode.desktop*`),
//! so the two apps never share files. `RUNE_DIRS_ROOT=<root>` moves every folder this app writes
//! under `<root>` for an isolated run.
//!
//! A debug build keeps its own folders, `rune-opencode.debug` in place of `rune-opencode`, unless
//! `RUNE_DIRS_ROOT` places them: a development or harness run never reads or writes the profile a
//! release build keeps, as OpenCode Desktop's unpackaged runs keep `ai.opencode.desktop.dev`
//! apart from the release's (`packages/desktop/src/main/constants.ts`, `APP_ID`). A pull
//! request's build (`anomaly.opencode-rune.pr.<N>`) keeps `rune-opencode.pr-<N>`, as each of
//! OpenCode Desktop's channels has its own `userData`: installing, running or uninstalling one
//! never touches another's profile (a `RUNE_DIRS_ROOT` run, apart already, keeps `rune-opencode`
//! under its root). Stable and beta builds keep `rune-opencode.stable` and `rune-opencode.beta`
//! (apps of their own beside dev's, as OpenCode Desktop's Dev, Beta and production apps are);
//! dev keeps `rune-opencode`. The browser engine's folder stays shared.

use std::path::PathBuf;
use std::sync::OnceLock;

use rune::dirs::AppDirs;

use crate::App;

const APP_ID: &str = "shuvcode-rune";
const DEBUG_APP_ID: &str = "shuvcode-rune.debug";

/// The app's folders, for code that removes them (`install`).
pub(crate) fn app() -> Option<&'static AppDirs> {
    static DIRS: OnceLock<Option<AppDirs>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let dirs = AppDirs::new(APP_ID)?;
        // A `RUNE_DIRS_ROOT` run is apart already, and the harness seeds `rune-opencode` there.
        if dirs.is_rooted() {
            return Some(dirs);
        }
        AppDirs::new(if cfg!(debug_assertions) { DEBUG_APP_ID } else { release_id() })
    })
    .as_ref()
}

/// A release build's folders' name: `rune-opencode`, or a pull request's build's own
/// `rune-opencode.pr-<N>` (its bundle id, `anomaly.opencode-rune.pr.<N>`, is set at build time).
fn release_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(pr_id)
}

fn pr_id() -> String {
    match option_env!("OPENCODE_RUNE_BUNDLE_ID").and_then(|id| id.strip_prefix("dev.shuv.shuvcode-rune.pr.")) {
        Some(n) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => format!("{APP_ID}.pr-{n}"),
        // Stable and beta are apps of their own beside dev's (`win_install::side_by_side`), each
        // with its own `userData` as OpenCode Desktop's channels have; dev keeps `shuvcode-rune`.
        _ => match option_env!("OPENCODE_RUNE_BUILD_CHANNEL") {
            Some(c @ ("stable" | "beta")) => format!("{APP_ID}.{c}"),
            _ => APP_ID.to_owned(),
        },
    }
}

/// Whether `RUNE_DIRS_ROOT` put every folder this app writes under one scratch root.
pub fn rooted() -> bool {
    app().is_some_and(AppDirs::is_rooted)
}

/// The app's database (`opencode.sqlite`) and the files it imported from, retired beside it.
pub fn data() -> Option<PathBuf> {
    Some(app()?.data().to_owned())
}

/// User configuration: `plugins-override/` (the same folder as `data` on macOS and Windows).
pub fn config() -> Option<PathBuf> {
    Some(app()?.config().to_owned())
}

/// Rebuildable caches; safe to delete.
pub fn cache() -> Option<PathBuf> {
    Some(app()?.cache().to_owned())
}

/// The log folder: the motion log and frame diagnostics (`perf/`), what Export logs packs.
/// macOS keeps the folder it always had.
pub fn logs() -> Option<PathBuf> {
    let app = app()?;
    Some(if cfg!(target_os = "macos") { app.sibling("shuvcode-rune").logs().to_owned() } else { app.logs().to_owned() })
}

/// The motion log (`motion.jsonl`).
pub fn motion_log() -> Option<PathBuf> {
    Some(logs()?.join("motion.jsonl"))
}

/// Where the updater downloads a release and keeps its records: the team bundle's caches on
/// macOS (the folder it always had; stable's and beta's bundles their own), the app's cache folder
/// elsewhere.
pub fn updates() -> Option<PathBuf> {
    let app = app()?;
    let bundle = match option_env!("OPENCODE_RUNE_BUILD_CHANNEL") {
        Some(c @ ("stable" | "beta")) => format!("dev.shuv.shuvcode-rune.{c}"),
        _ => "dev.shuv.shuvcode-rune".to_owned(),
    };
    let cache = if cfg!(target_os = "macos") { app.sibling(&bundle).cache().to_owned() } else { app.cache().to_owned() };
    Some(cache.join("Updates"))
}

/// OpenCode Desktop's Electron `userData` folders (production, Beta, Dev), whose `drafts.sqlite`
/// seeds a fresh profile, read-only, after the first frame.
pub fn official() -> Vec<PathBuf> {
    let Some(app) = app() else { return Vec::new() };
    ["ai.opencode.desktop", "ai.opencode.desktop.beta", "ai.opencode.desktop.dev"].iter().map(|n| app.sibling(n).config().to_owned()).collect()
}

/// The browser engine's folders (its installed versions, browser profiles and sockets), shared
/// with every app that shows a browser pane: beside this app's, so `RUNE_DIRS_ROOT` moves them too.
#[cfg(any(target_os = "macos", windows))]
pub fn engine() -> Option<AppDirs> {
    Some(app()?.sibling("co.anomaly.opencode"))
}

/// The folder the Hex dictation app writes its endpoint file to.
#[cfg(not(target_arch = "wasm32"))]
pub fn hex_support() -> Option<PathBuf> {
    Some(app()?.sibling("hex").data().to_owned())
}

/// Where Export saves a session.
pub fn downloads() -> Option<PathBuf> {
    Some(app()?.downloads()?.to_owned())
}

/// The user's home folder as text (`USERPROFILE` on Windows), or empty when there is none.
pub fn home() -> String {
    app().and_then(AppDirs::home).map(|h| h.to_string_lossy().into_owned()).unwrap_or_default()
}

impl App {
    /// `dir` on `server` with that server's home folder shown as `~`: this machine's (the
    /// fixture's, in a fixture run) for the app's own server, another's as it reported it
    /// (`servers::health::home`), else the folder as it is (`previewPath`, `tab-nav.tsx:146-155`).
    pub(crate) fn tilde(&self, server: &opencode_kit::servers::ServerKey, dir: &str) -> String {
        if server.is_sidecar() {
            return opencode_kit::path::tilde(dir, &self.home);
        }
        crate::servers::health::home(server).map_or_else(|| dir.to_owned(), |home| opencode_kit::path::tilde(dir, &home))
    }
}
