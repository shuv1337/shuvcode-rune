//! The engine's availability state, its failure messages, and, with the `engine` feature, where
//! installed engines live (`browser-engine\<version>\OpenCode Browser Engine`), which one runs,
//! and the socket the app talks to it on. `source` says where a new one comes from and which
//! ones this build accepts, `download` and `unpack` fetch and install it, `prefetch` runs the job.

#[cfg(feature = "engine")]
use std::path::{Path, PathBuf};
use rune::time::Instant;

#[cfg(feature = "engine")]
use sha2::{Digest, Sha256};

/// The engine programs this build speaks to: an engine's version is CEF's (`156.0.26`) and this
/// revision (`156.0.26.1`). Bump it when the app needs an engine built from newer engine code (a
/// protocol change); a release of a newer CEF with the same revision is an update for every app
/// that accepts it.
pub const ENGINE_REVISION: u32 = 25;
#[cfg(not(any(windows, target_os = "linux")))]
pub const ENGINE_BUNDLE_NAME: &str = "OpenCode Browser Engine.app";
#[cfg(not(any(windows, target_os = "linux")))]
pub const ENGINE_BIN_REL: &str = "Contents/MacOS/opencode-browser-engine";
#[cfg(any(windows, target_os = "linux"))]
pub const ENGINE_BUNDLE_NAME: &str = "OpenCode Browser Engine";
#[cfg(target_os = "linux")]
pub const ENGINE_BIN_REL: &str = "opencode-browser-engine";
#[cfg(windows)]
pub const ENGINE_BIN_REL: &str = "opencode-browser-engine.exe";
/// Approximate archive size in decimal megabytes, shown before the first download (the index
/// gives the exact one once it's read): the 156.0.14.1 macOS zip is `128_330_085` bytes.
#[cfg(not(any(windows, target_os = "linux")))]
pub const ENGINE_DOWNLOAD_MB: u64 = 128;
/// Approximate decimal MB of the stripped 156.0.26.1 linux-x64 tar.zst.
#[cfg(target_os = "linux")]
pub const ENGINE_DOWNLOAD_MB: u64 = 131;
/// Approximate archive size in decimal megabytes: `package-windows.ps1`'s x64 archive of
/// 156.0.26.1 is `141_657_795` bytes.
#[cfg(windows)]
pub const ENGINE_DOWNLOAD_MB: u64 = 142;

/// The file in a version's folder naming the trust rule that installed it (`source::marker`).
#[cfg(feature = "engine")]
pub(crate) const TRUST_MARKER: &str = "trust";

/// The OS named no folder for the app's data (`Fault::Dirs`).
#[cfg(feature = "engine")]
pub(crate) const NO_DIRS: &str = "the OS has no folder for the browser's data";

/// A development override from the environment (`OPENCODE_BROWSER_ENGINE_BIN`, `_APP`,
/// `OPENCODE_BROWSER_SOCKET`): read only in a debug build, as
/// `source::inputs` reads `_SOURCE` and `_TRUST`, so a variable can't make a shipped app run an
/// engine past its trust rule (whoever sets one could also pass a headless flag).
pub(crate) fn dev_override(name: &str) -> Option<std::ffi::OsString> {
    cfg!(debug_assertions)
        .then(|| std::env::var_os(name))
        .flatten()
        .filter(|v| !v.is_empty())
}

/// What went wrong getting the engine or keeping it running: each one names its cause and what
/// the user can do, and the app words it from its catalog (`Display` is the English the logs and
/// the agent tools read).
#[derive(Clone, Debug, PartialEq)]
pub enum Fault {
    /// This build has no engine download for its platform, or its source lists none.
    NotPublished,
    /// The download's server couldn't be reached.
    Unreachable { host: String, why: Net },
    /// The server, a proxy or a filter refused the download (HTTP 401, 403, 407, 451).
    Blocked { host: String, status: u16 },
    /// The server answered with another error (404: the address is dead; 5xx: it's failing).
    Server { host: String, status: u16 },
    /// Something answered in the source's place with a page that isn't its index (a network's
    /// sign-in page, a filter).
    Unexpected { host: String },
    /// The connection ended part-way: `have` of `total` bytes are kept, and Retry goes on from there.
    Interrupted { have: u64, total: u64 },
    /// The bytes aren't the archive the index names (its size or SHA-256 differ, or it doesn't
    /// unpack): they're deleted, and Retry downloads it again.
    Corrupt,
    /// The archive isn't one this build trusts.
    Untrusted(Distrust),
    /// Before the download: the rest of it and what it unpacks to don't fit.
    NoSpace { need: u64, free: u64 },
    /// A write found the disk full.
    DiskFull,
    /// Installing couldn't get the memory it needs (a computer out of commit): the bytes so far
    /// stay, and Retry goes on from there.
    NoMemory,
    /// Writing the engine's files failed for another reason, in the OS's words.
    Write(String),
    /// The engine's folders couldn't be found or made (`what` says which, in English).
    Dirs(&'static str),
    /// Another app on this computer downloads the engine into the same folder right now.
    Busy,
    /// The OS refused to start the engine, in its words.
    Launch(String),
    /// Windows or security software stopped the engine from starting (an antivirus quarantine,
    /// Smart App Control, a policy), in the OS's words.
    Prevented(String),
    /// The installed engine's files are missing or damaged (the loader's exit code): the version
    /// is removed, and Retry downloads it again.
    Damaged(u32),
    /// The engine ended before it was ready, with this exit code.
    Exited(Option<u32>),
    /// The engine stopped after it ran (it crashed or was ended).
    Stopped,
    /// The engine ended because the computer had no memory left to give it.
    OutOfMemory,
    /// The engine runs but doesn't answer (Retry ends it and starts a new one).
    Hung,
}

/// Why a server couldn't be reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Net {
    /// Its name didn't resolve (offline, or DNS).
    NotFound,
    /// Nothing answered on its address.
    Refused,
    TimedOut,
    /// The secure connection failed (a proxy or security software that inspects TLS, a clock).
    Secure,
    /// The proxy failed.
    Proxy,
    Other,
}

/// Why an archive isn't trusted.
#[derive(Clone, Debug, PartialEq)]
pub enum Distrust {
    /// A program in it is signed by someone else.
    Signer { file: String, signer: String },
    /// A program in it has no valid signature.
    Unsigned { file: String },
    /// Where it came from or its hash isn't what this build accepts.
    Rule,
}

impl Fault {
    /// Whether Retry can help: a build with nothing published can't.
    pub fn retryable(&self) -> bool {
        !matches!(self, Self::NotPublished)
    }

    /// `url`'s host, as a fault names it.
    #[cfg(feature = "engine")]
    pub(crate) fn host(url: &str) -> String {
        let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
        rest.split(['/', '?', '#']).next().unwrap_or_default().to_owned()
    }
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mb = |b: &u64| b.div_ceil(1_000_000);
        match self {
            Self::NotPublished => write!(f, "this build has no browser engine download for its platform"),
            Self::Unreachable { host, why } => write!(f, "couldn't reach {host} ({why:?})"),
            Self::Blocked { host, status } | Self::Server { host, status } => write!(f, "{host} answered HTTP {status}"),
            Self::Unexpected { host } => write!(f, "{host} answered with a page that isn't the engine's index"),
            Self::Interrupted { have, total } => write!(f, "the download stopped at {} of {} MB", mb(have), mb(total)),
            Self::Corrupt => write!(f, "the download didn't match its size and SHA-256"),
            Self::Untrusted(Distrust::Signer { file, signer }) => write!(f, "{file} is signed by {signer}"),
            Self::Untrusted(Distrust::Unsigned { file }) => write!(f, "{file} has no valid signature"),
            Self::Untrusted(Distrust::Rule) => write!(f, "the archive isn't one this build trusts"),
            Self::NoSpace { need, free } => write!(f, "needs {} MB, {} MB free", mb(need), mb(free)),
            Self::DiskFull => write!(f, "the disk is full"),
            Self::NoMemory => write!(f, "not enough memory to install the engine"),
            Self::Write(why) => write!(f, "couldn't write the engine's files: {why}"),
            Self::Dirs(what) => write!(f, "{what}"),
            Self::Busy => write!(f, "another app is downloading the engine"),
            Self::Launch(why) => write!(f, "couldn't start the engine: {why}"),
            Self::Prevented(why) => write!(f, "the engine was prevented from starting: {why}"),
            Self::Damaged(code) => write!(f, "the installed engine is damaged (exit code {code:#010x})"),
            Self::Exited(Some(code)) => write!(f, "the engine ended while starting (exit code {code:#010x})"),
            Self::Exited(None) => write!(f, "the engine ended while starting"),
            Self::Stopped => write!(f, "the engine stopped unexpectedly"),
            Self::OutOfMemory => write!(f, "the engine ran out of memory"),
            Self::Hung => write!(f, "the engine isn't responding"),
        }
    }
}

/// The engine's availability, one state machine that the pane and Settings read:
///
/// ```text
/// NotInstalled ─► Downloading ─► Verifying ─► Installed ─► Connecting ─► Ready
///                  │   ▲    │                     ▲            │
///                  ▼   │    ▼                     └─ found on disk at launch
///             Paused(reason)  Failed(retry_at) ─► Downloading (at retry_at, or Retry)
///                                  ▲
///                                  └─ the engine process went away, or stopped answering
///                                     (Retry starts it again; a late answer takes it back)
/// ```
///
/// `Downloading` is the download a page waits for (full speed, one try). A launch that finds no
/// engine this build accepts downloads it quietly once the app is up (`BrowserHost::fetch_in_background`),
/// and an update downloads beside the running engine, once a run after its first page opens:
/// neither shows here, until a page opens and joins it. Nothing of this runs before the app has
/// drawn and takes input: no request, timer or thread.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineStatus {
    /// This build has no engine for its platform (it is built without the `engine` feature, or
    /// for a platform without a Unix-socket engine), so the pane never downloads or starts one.
    Unsupported,
    /// Not looked for by this host yet: a page that opens (or BrowserHost::prepare) finds out.
    /// The launch's quiet fetch leaves it so: what it installs is found when a page looks.
    Unchecked,

    NotInstalled,
    Downloading { downloaded_bytes: u64, total_bytes: Option<u64> },
    /// The archive is in; its hash and signatures are checked and it moves into place.
    Verifying,
    /// The background download waits for the OS to allow it. Opening the pane goes ahead anyway.
    Paused(PauseReason),
    /// On disk, not running.
    Installed,
    Connecting,
    Ready,
    Failed(Failure),
}

/// Why a download or start failed, and when the background download tries again (`None`: it
/// waits for Retry).
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub fault: Fault,
    pub retry_at: Option<Instant>,
}

impl EngineStatus {
    /// The engine runs but didn't answer a request in time.
    pub fn is_hung(&self) -> bool {
        matches!(self, Self::Failed(f) if f.fault == Fault::Hung)
    }

    /// The source publishes no engine this build accepts for this platform.
    pub fn is_not_published(&self) -> bool {
        matches!(self, Self::Failed(f) if f.fault == Fault::NotPublished)
    }
}

impl Failure {
    pub fn retrying(fault: Fault, at: Instant) -> Self {
        Self { fault, retry_at: Some(at) }
    }
}

impl From<Fault> for Failure {
    fn from(fault: Fault) -> Self {
        Self { fault, retry_at: None }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fault.fmt(f)
    }
}

/// Why the background download is paused, as the OS reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseReason {
    /// The connection is metered (or roaming, or over its data limit).
    Metered,
    /// The computer runs on its battery.
    OnBattery,
    /// The computer saves energy (Windows' battery saver, macOS's Low Power Mode).
    BatterySaver,
}

impl PauseReason {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Metered => "Paused on a metered connection",
            Self::OnBattery => "Paused on battery power",
            Self::BatterySaver => "Paused while the computer saves energy",
        }
    }
}

/// An engine's version: CEF's three numbers, then the engine revision (`ENGINE_REVISION`).
/// Written `156.0.26.1` (its folder) or `156.0.26-1` (the index's release version).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub cef: [u32; 3],
    pub revision: u32,
}

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.trim().split(['.', '-']).map(|p| p.parse::<u32>().ok());
        let v = Self { cef: [parts.next()??, parts.next()??, parts.next()??], revision: parts.next()?? };
        parts.next().is_none().then_some(v)
    }

    /// Whether this build speaks to an engine of this version.
    pub fn accepted(self) -> bool {
        self.revision == ENGINE_REVISION
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c] = self.cef;
        write!(f, "{a}.{b}.{c}.{}", self.revision)
    }
}

/// Where everything of the engine lives, the third input beside the source and the trust rule:
/// its state folder holds the installed versions and each app's browser profile, its cache
/// folder the sockets. The app passes the folders its own data root decides (`set_home`), so a
/// scratch profile gets a scratch engine and a real install the real one; without it they are
/// `rune::dirs`' for `co.anomaly.opencode`, shared by every app that shows a browser pane
/// (`~/Library/Application Support/co.anomaly.opencode` on macOS, `%LOCALAPPDATA%\co.anomaly.opencode`
/// on Windows).
#[cfg(feature = "engine")]
static HOME: std::sync::OnceLock<rune::dirs::AppDirs> = std::sync::OnceLock::new();

/// Sets the engine's folders, once, before any page opens.
#[cfg(feature = "engine")]
pub fn set_home(dirs: rune::dirs::AppDirs) {
    HOME.set(dirs).ok();
}

/// The app quits: an engine download that runs in the background stops where it is, its threads
/// get normal priority back for what they still do, and it closes the files it writes
/// ([`quit_wait`] waits for that).
pub fn quit() {
    #[cfg(feature = "engine")]
    crate::prefetch::Prefetch::global().quit();
}

/// The quit's last step, after [`quit`]: waits, half a second at most, until the engine's download
/// has closed the files it writes, so the process's end has none of them to close (Windows
/// Defender scans a half-written DLL inside that close, for seconds). False when they are still
/// open.
pub fn quit_wait() -> bool {
    #[cfg(feature = "engine")]
    {
        crate::prefetch::Prefetch::global().wait_closed()
    }
    #[cfg(not(feature = "engine"))]
    {
        true
    }
}

#[cfg(feature = "engine")]
pub(crate) fn engine_dirs() -> Option<rune::dirs::AppDirs> {
    HOME.get().cloned().or_else(|| rune::dirs::AppDirs::new("co.anomaly.opencode"))
}

/// The folder holding one folder per installed engine version.
#[cfg(feature = "engine")]
pub fn engines_root() -> PathBuf {
    try_engines_root().unwrap_or_else(|| fallback_dir().join("browser-engine"))
}

#[cfg(feature = "engine")]
pub(crate) fn fallback_dir() -> PathBuf {
    #[cfg(unix)]
    return std::env::temp_dir().join(format!("co.anomaly.opencode-{}", user_id()));
    #[cfg(not(unix))]
    std::env::temp_dir().join("co.anomaly.opencode")
}

#[cfg(all(feature = "engine", unix))]
#[allow(unsafe_code)]
fn user_id() -> u32 {
    // SAFETY: geteuid takes no pointers and returns the effective uid of this process.
    unsafe { libc::geteuid() }
}

#[cfg(feature = "engine")]
pub fn try_engines_root() -> Option<PathBuf> {
    Some(engine_dirs()?.state().join("browser-engine"))
}

/// An installed engine: its version and its executable.
#[cfg(feature = "engine")]
#[derive(Clone, Debug)]
pub struct Installed {
    pub version: Option<Version>,
    pub bin: PathBuf,
}

/// The engine to start: `OPENCODE_BROWSER_ENGINE_BIN` (an executable) or
/// `OPENCODE_BROWSER_ENGINE_APP` (its folder) when set, a development build's own, and otherwise
/// the newest installed version this build accepts. Reads one folder; call it off the UI thread.
#[cfg(feature = "engine")]
pub fn installed() -> Option<Installed> {
    if let Some(bin) = dev_override("OPENCODE_BROWSER_ENGINE_BIN").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(Installed { version: None, bin });
    }
    if let Some(bin) = dev_override("OPENCODE_BROWSER_ENGINE_APP").map(|app| PathBuf::from(app).join(ENGINE_BIN_REL)).filter(|p| p.is_file()) {
        return Some(Installed { version: None, bin });
    }
    let root = try_engines_root()?;
    // Only a complete version installed under a rule this build accepts: a release build never
    // starts an engine an older build took from the loopback stand-in, one it can't tell the
    // origin of, or a half-installed folder missing its executable (which must not hide an older
    // intact version).
    installed_versions(&root)
        .into_iter()
        .filter(|v| {
            let dir = root.join(v.to_string());
            v.accepted() && is_complete(&dir) && crate::source::accepts_install(&dir)
        })
        .max()
        .map(|version| Installed { version: Some(version), bin: bin_in(&root.join(version.to_string())) })
}

/// True when `version_dir` is a non-link directory holding every platform entry binary required
/// for a complete install (`#617` half-installed recovery + Windows `bootstrap.exe` + client DLL).
#[cfg(feature = "engine")]
pub(crate) fn is_complete(version_dir: &Path) -> bool {
    if is_reparse_or_symlink(version_dir) || is_reparse_or_symlink(&version_dir.join(ENGINE_BUNDLE_NAME)) {
        return false;
    }
    let bin = bin_in(version_dir);
    if is_reparse_or_symlink(&bin) || !bin.is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        let dll = bin.with_file_name("opencode-browser-engine.dll");
        if is_reparse_or_symlink(&dll) || !dll.is_file() {
            return false;
        }
    }
    #[cfg(target_os = "linux")]
    {
        let helper = bin.with_file_name("opencode-browser-engine-helper");
        if is_reparse_or_symlink(&helper) || !helper.is_file() {
            return false;
        }
    }
    true
}

/// The versions with a folder in `root`, accepted by this build or not.
#[cfg(feature = "engine")]
pub(crate) fn installed_versions(root: &Path) -> Vec<Version> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    entries.flatten().filter_map(|e| e.file_name().to_str().and_then(Version::parse)).collect()
}

/// The executable inside a version's folder.
#[cfg(feature = "engine")]
pub(crate) fn bin_in(version_dir: &Path) -> PathBuf {
    version_dir.join(ENGINE_BUNDLE_NAME).join(ENGINE_BIN_REL)
}

/// Removes every version folder but `keep` and the newest that each other app revision would
/// start (Linux: keep the current version and the newest other version), and unpacking folders left by an app that quit mid-install (their process is gone, so
/// another app's install in progress keeps its own). A folder an engine still runs from stays:
/// it moves aside first, which Windows refuses while its files are open, so nothing in use is
/// ever half deleted. Reads one folder; call it off the UI thread.
#[cfg(feature = "engine")]
pub(crate) fn remove_old(root: &Path, keep: Version) {
    let versions = installed_versions(root);
    #[cfg(target_os = "linux")]
    let other = versions.iter().filter(|&&v| v != keep).max();
    #[cfg(not(target_os = "linux"))]
    let newest_of = |revision: u32| versions.iter().filter(|v| v.revision == revision).max().copied();
    for v in &versions {
        #[cfg(not(target_os = "linux"))]
        if *v == keep || (v.revision != keep.revision && newest_of(v.revision) == Some(*v)) || *v > keep {
            continue;
        }
        let dir = root.join(v.to_string());
        if is_reparse_or_symlink(&dir) {
            continue;
        }
        #[cfg(target_os = "linux")]
        let _executables = {
            if Some(v) == other || *v == keep { continue; }
            let bin = bin_in(&dir);
            // Linux denies a write-open on a running ELF (ETXTBSY), and denies exec while a
            // write descriptor is held. Hold both through removal; this covers older engines
            // too, even when launched directly or by an app that has since exited.
            let open = |path| std::fs::OpenOptions::new().write(true).open(path);
            let Ok(main) = open(bin.clone()) else { continue };
            let Ok(helper) = open(bin.with_file_name("opencode-browser-engine-helper")) else { continue };
            (main, helper)
        };
        let seq = DISCARD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let trash = root.join(format!(".{v}.old-{}-{seq}", std::process::id()));
        if std::fs::rename(&dir, &trash).is_ok() {
            std::fs::remove_dir_all(&trash).ok();
        }
    }
    sweep_unpacking(root);
}

/// Removes the unpacking and removal folders a process left behind when it quit (or was killed)
/// mid-way; each one is as large as an engine.
#[cfg(feature = "engine")]
pub(crate) fn sweep_unpacking(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // `.<version>.staging-<pid>-<n>` and `.<version>.old-<pid>[-<n>]`: the process that made it is gone.
        let pid = name.strip_prefix('.').and_then(|name| {
            let (version, pid) = if let Some((version, rest)) = name.split_once(".staging-") {
                let (pid, seq) = rest.split_once('-')?;
                seq.parse::<u64>().ok()?;
                (version, pid)
            } else {
                let (version, rest) = name.split_once(".old-")?;
                let pid = match rest.split_once('-') {
                    Some((pid, seq)) => {
                        seq.parse::<u64>().ok()?;
                        pid
                    }
                    None => rest,
                };
                (version, pid)
            };
            Version::parse(version)?;
            pid.parse::<u32>().ok()
        });
        if pid.is_some_and(|pid| pid != std::process::id() && !crate::background::pid_alive(pid)) {
            let path = entry.path();
            if !is_reparse_or_symlink(&path) {
                std::fs::remove_dir_all(path).ok();
            }
        }
    }
}

/// Removes the partial downloads nobody has touched for a day: an earlier version's, left when
/// the app was updated before its engine arrived, each up to an archive's size. The one a
/// download writes is locked (`download::Body`), also by another app or an older build beside
/// this one, and stays; so does a recent one that such a build goes on from at its next try.
#[cfg(feature = "engine")]
pub(crate) fn sweep_partials(root: &Path) {
    const STALE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        // `.<version>-<16 hex of the archive's hash>.part`
        let ours = name.to_str().and_then(|name| name.strip_prefix('.')?.strip_suffix(".part")?.rsplit_once('-')).is_some_and(|(version, hash)| {
            Version::parse(version).is_some() && hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())
        });
        let path = entry.path();
        let stale = entry.metadata().and_then(|m| m.modified()).is_ok_and(|at| at.elapsed().is_ok_and(|age| age >= STALE));
        if !ours || !stale || is_reparse_or_symlink(&path) {
            continue;
        }
        let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else { continue };
        // Removed while the lock is held, so no download starts on it in between.
        if file.try_lock().is_ok() {
            std::fs::remove_file(&path).ok();
        }
    }
}

/// The socket for this app. Connections use `try_socket_path` to report directory errors.
#[cfg(feature = "engine")]
pub fn socket_path() -> PathBuf {
    try_socket_path().unwrap_or_else(|_| fallback_dir().join(socket_name()))
}

#[cfg(feature = "engine")]
pub fn try_socket_path() -> Result<PathBuf, &'static str> {
    if let Some(custom) = dev_override("OPENCODE_BROWSER_SOCKET") {
        return custom_socket(Path::new(&custom));
    }
    let dirs = engine_dirs().ok_or(NO_DIRS)?;
    let name = socket_name();
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::DirBuilderExt;
        // The cache may already be shared with other callers. Create a dedicated socket
        // directory with private permissions from the outset, and validate it before use.
        let dir = dirs.cache().join("ipc");
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|_| "Couldn't create the browser socket directory.")?;
        let dir = dir.canonicalize().map_err(|_| "Couldn't resolve the browser socket directory.")?;
        custom_socket(&dir.join(name)).map_err(|_| "The browser IPC directory must be owned by you with mode 0700 and have secure parent directories.")
    }
    #[cfg(not(target_os = "linux"))]
    {
        create_private_dir(dirs.cache()).ok();
        fit_socket(dirs.cache().join(name))
    }
}

/// The longest path a socket binds: `sun_path` holds 104 bytes on macOS and 108 on Windows, with
/// the NUL. Past it the engine can't listen and ends while starting. (Linux keeps its path: the
/// same limit applies there, but nothing has run this on Linux yet.)
#[cfg(all(feature = "engine", not(target_os = "linux")))]
const SOCKET_PATH_MAX: usize = if cfg!(target_os = "macos") { 103 } else { 107 };

/// `path`, or when it is too long for a socket (a long home folder or `RUNE_DIRS_ROOT`: the
/// cache's own part of it is about 60 bytes on macOS), `s-<hash of path>.sock` in a folder only
/// this user can open under the system's short per-user temporary folder. A path that fits stays
/// where every app and engine before this looked; a long one never worked, so no running engine
/// is lost. The hash keeps two apps, or one app with two homes, apart.
#[cfg(all(feature = "engine", not(target_os = "linux")))]
fn fit_socket(path: PathBuf) -> Result<PathBuf, &'static str> {
    const ERR: &str = "Couldn't create the browser socket directory.";
    if path.as_os_str().len() <= SOCKET_PATH_MAX {
        return Ok(path);
    }
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    let hash: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let dir = short_socket_dir();
    create_private_dir(&dir).map_err(|_| ERR)?;
    let short = dir.join(format!("s-{hash}.sock"));
    if short.as_os_str().len() > SOCKET_PATH_MAX {
        return Err(ERR);
    }
    Ok(short)
}

/// macOS: `TMPDIR` (the system's per-user `/var/folders/…/T`, private to the user and about 50
/// bytes, unless it was moved), or when that is too long too, the folder the system names.
#[cfg(all(feature = "engine", target_os = "macos"))]
#[allow(unsafe_code)]
fn short_socket_dir() -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    // `/s-<16 hex>.sock` inside `opencode-browser`.
    const LEAF: usize = 24;
    let tmp = std::env::temp_dir();
    let candidate = tmp.join("opencode-browser");
    if candidate.as_os_str().len() + LEAF <= SOCKET_PATH_MAX {
        return candidate;
    }
    let mut buf = [0u8; 1024];
    // SAFETY: confstr writes at most `buf.len()` bytes, NUL included, and returns the length the
    // whole value needs (0 on failure).
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), buf.len()) };
    let base = if n > 1 && n <= buf.len() { PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..n - 1])) } else { tmp };
    base.join("opencode-browser")
}

/// Windows: the user's own temporary folder, or when its path is too long (a long user folder name,
/// or a short one in a script whose UTF-8 bytes add up: Cyrillic, CJK), the same folder by its 8.3
/// name where the volume keeps them, or a short folder only this user can open (`private_dir`)
/// where it doesn't. Without them such a user had no browser at all: "Couldn't create the browser
/// socket directory."
#[cfg(all(feature = "engine", windows))]
#[allow(unsafe_code)]
fn short_socket_dir() -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    // `\s-<16 hex>.sock` inside `opencode-browser`.
    const LEAF: usize = 24;
    let tmp = std::env::temp_dir();
    let candidate = tmp.join("opencode-browser");
    if candidate.as_os_str().len() + LEAF <= SOCKET_PATH_MAX {
        return candidate;
    }
    let wide: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut buf = [0u16; 1024];
    // SAFETY: `wide` is a NUL-terminated path that outlives the call; the call writes at most
    // `buf.len()` units, NUL included, and returns the length written without it (0 on failure, or
    // the size it needs when `buf` is too small).
    let n = unsafe { windows::Win32::Storage::FileSystem::GetShortPathNameW(windows::core::PCWSTR(wide.as_ptr()), Some(&mut buf)) } as usize;
    let base = if n > 0 && n < buf.len() { PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])) } else { tmp };
    let short = base.join("opencode-browser");
    if short.as_os_str().len() + LEAF <= SOCKET_PATH_MAX {
        return short;
    }
    // No 8.3 name either (8.3 off, ReFS, a Dev Drive): a short folder only this user can open.
    crate::private_dir::private_short_dir().unwrap_or(short)
}

/// The engine chmods its socket's parent. On Linux, only pass an override in an already private
/// directory, using a path that another user can't replace between this check and spawn.
#[cfg(feature = "engine")]
pub(crate) fn custom_socket(path: &Path) -> Result<PathBuf, &'static str> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        const ERR: &str = "Choose an existing private directory (mode 0700, no symlinks) for the browser socket.";
        let path = std::path::absolute(path).map_err(|_| ERR)?;
        let parent = path.parent().ok_or(ERR)?;
        let uid = user_id();
        let mut prefix = PathBuf::new();
        for component in parent.components() {
            prefix.push(component);
            let meta = prefix.symlink_metadata().map_err(|_| ERR)?;
            if !meta.is_dir() || (meta.uid() != 0 && meta.uid() != uid) {
                return Err(ERR);
            }
            if prefix == parent {
                if meta.uid() != uid || meta.mode() & 0o7777 != 0o700 {
                    return Err(ERR);
                }
            } else if meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0 {
                // A sticky ancestor (e.g. /tmp) protects the next owned directory.
                return Err(ERR);
            }
        }
        path.file_name().ok_or(ERR)?;
        Ok(path)
    }
    #[cfg(not(target_os = "linux"))]
    Ok(path.to_owned())
}

/// The engine serves one app at a time: the last to connect takes every frame and tab update,
/// and every app numbers its tabs from 1. So each app (by executable path) runs its own engine.
#[cfg(feature = "engine")]
pub fn socket_name_for(exe: &Path, uid: u32) -> String {
    format!("browser-{uid}-{}.sock", exe_tag(exe))
}

#[cfg(feature = "engine")]
fn socket_name() -> String {
    #[cfg(unix)]
    let uid = user_id();
    #[cfg(windows)]
    let uid = 0;
    socket_name_for(&std::env::current_exe().unwrap_or_default(), uid)
}

/// Eight hex digits of the executable path's hash, pairing an app's socket with its profile.
#[cfg(feature = "engine")]
pub(crate) fn exe_tag(exe: &Path) -> String {
    let digest = Sha256::digest(exe.as_os_str().as_encoded_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(all(feature = "engine", not(target_os = "linux")))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// A failed write's fault: no space left, or the OS's words for what else went wrong (access
/// denied, a path too long, a file held open).
#[cfg(feature = "engine")]
pub(crate) fn err_write(err: &std::io::Error) -> Fault {
    // ERROR_HANDLE_DISK_FULL, ERROR_DISK_FULL on Windows; ENOSPC, EDQUOT elsewhere.
    #[cfg(windows)]
    let full = matches!(err.raw_os_error(), Some(39 | 112));
    #[cfg(unix)]
    let full = matches!(err.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT));
    if full || err.kind() == std::io::ErrorKind::StorageFull { Fault::DiskFull } else { Fault::Write(err.to_string()) }
}

/// True when `path` itself is a symlink or (on Windows) any reparse point such as a junction.
#[cfg(feature = "engine")]
pub(crate) fn is_reparse_or_symlink(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return false };
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(feature = "engine")]
static DISCARD_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Removes an installed version whose files are damaged or incomplete, so the next start downloads
/// it again. Never follows symlinks or Windows junctions. A folder an engine still runs from stays
/// (Windows refuses the move while its files are open).
#[cfg(feature = "engine")]
pub(crate) fn discard(root: &Path, version: Version) {
    let dir = root.join(version.to_string());
    let Ok(meta) = std::fs::symlink_metadata(&dir) else { return };
    if is_reparse_or_symlink(&dir) {
        return;
    }
    if !meta.is_dir() {
        std::fs::remove_file(&dir).ok();
        return;
    }
    let seq = DISCARD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let trash = root.join(format!(".{version}.old-{}-{seq}", std::process::id()));
    if std::fs::rename(&dir, &trash).is_ok() {
        std::fs::remove_dir_all(&trash).ok();
    }
}
