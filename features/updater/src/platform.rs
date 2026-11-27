//! What the machine's platform effects run (Desktop's `platform.ts`): blocking, on the updater's
//! worker thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::machine::Target;

pub trait Platform: Send + Sync + 'static {
    /// The running build's version, as the feed names releases.
    fn version(&self) -> &str;

    /// The release to install, `None` when the running build is current.
    fn check(&self) -> Result<Option<Target>, String>;

    /// Downloads, verifies and stages the release the last check found; `progress` hears the
    /// download's bytes so far and its total.
    fn stage(&self, progress: &dyn Fn(u64, u64)) -> Result<(), String>;

    /// Hands off to the staged update; once this returns `Ok` the app must quit.
    fn install(&self) -> Result<(), String>;

    /// After `install`, the app starts the update itself by quitting and starting again
    /// (`Tx::relaunch`, the app menu's Restart), rather than the hand-off starting it (Windows'
    /// helper puts the update in place only after the app exits, then starts it).
    fn relaunches(&self) -> bool {
        false
    }

    /// Whether quitting with an update staged installs it on the way out (`platform.ts:30`:
    /// `autoInstallOnAppQuit = process.platform === "darwin"` — macOS only).
    fn installs_on_quit(&self) -> bool {
        false
    }

    /// The app quits with an update staged: install it on the way out, without starting again.
    fn install_on_quit(&self) {}

    /// Install as soon as an update is ready.
    fn auto_install(&self) -> bool {
        false
    }

    /// Whether [`Platform::external_install`] can open a release's download (`externalInstall`).
    fn external(&self) -> bool {
        false
    }

    /// Opens a [`Target::External`] release's download for the user to install.
    fn external_install(&self, _url: &str) -> Result<(), String> {
        Err("External installer is unavailable".into())
    }

    /// Direct download link to offer when an in-app stage or install fails (Linux).
    fn fallback_download_url(&self) -> Option<String> {
        None
    }

    /// An install error an out-of-process helper recorded since the last run, taken once.
    fn take_last_error(&self) -> Option<String> {
        None
    }
}

#[inline]
fn fork_updater_enabled() -> bool {
    // shuv1337 fork: never check or install updates from Anomaly's `https://anoma.ly/release`
    // feed. Keeping the upstream platform updaters behind this gate ensures a Shuvcode (Rune)
    // build never prompts for or overwrites itself with upstream OpenCode (Rune).
    false
}

/// This build's updater, or `None` where none runs (a development build, a build without a
/// release feed, a bare `cargo` binary on macOS). Linux only checks: it offers the download.
pub fn make(exe: &Path, version: &str, build_channel: &str, updates: PathBuf) -> Option<Arc<dyn Platform>> {
    if !fork_updater_enabled() {
        let _ = (exe, version, build_channel, updates);
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (version, build_channel);
        super::mac::platform(exe, updates).map(|p| Arc::new(p) as Arc<dyn Platform>)
    }
    #[cfg(windows)]
    {
        super::windows::platform(exe, version, build_channel, updates).map(|p| Arc::new(p) as Arc<dyn Platform>)
    }
    #[cfg(target_os = "linux")]
    {
        super::linux::platform(exe, version, build_channel, updates).map(|p| Arc::new(p) as Arc<dyn Platform>)
    }
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    {
        let _ = (exe, version, build_channel, updates);
        None
    }
}
