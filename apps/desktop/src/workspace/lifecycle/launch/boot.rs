//! Process boot helpers, first server reads (`First::fetch`), probing, and post-connect hydration.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use opencode_kit::client::{Request, Server};
use opencode_kit::model::Transcript;
use rune::Tx;
use rune::time::Instant;
use serde_json::Value;

use crate::{App, Route, Source, arg, headless, session_pages};

static DEFERRED: AtomicBool = AtomicBool::new(false);

/// Windows: a release build is a windowed program (see `build.rs`), which Windows starts without
/// a console. Started from a terminal, it writes to that terminal's console (`--version`, logs);
/// output redirected to a file or a pipe already has somewhere to go.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
pub fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE};
    // SAFETY: reads this process's standard error handle, and attaches to its parent's console
    // when it has none; both fail harmlessly.
    unsafe {
        if GetStdHandle(STD_ERROR_HANDLE).is_null() {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

#[cfg(all(not(target_os = "windows"), not(target_arch = "wasm32")))]
pub fn attach_console() {}

/// In a build that records the profile-guided optimisation profile (`apps/desktop/scripts/pgo.ps1`), the counts:
/// LLVM's runtime writes them when the process ends normally, and the fast exit ends it with
/// `TerminateProcess`, which runs no exit handler.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_profile() {
    #[cfg(pgo_generate)]
    #[allow(unsafe_code)]
    {
        unsafe extern "C" {
            fn __llvm_profile_write_file() -> i32;
        }
        // SAFETY: LLVM's profiler runtime, linked into every `-Cprofile-generate` build; it only
        // writes the counters out.
        unsafe { __llvm_profile_write_file() };
    }
}

/// `opencode-desktop --warm-gpu`: what the setup runs once after an install, in a process nobody
/// waits for (`apps/installer` `first_start`). It makes the GPU device and Rune's pipelines
/// with no window (`rune::warm_gpu_cache`) and exits. Direct3D keeps what that leaves (its
/// shader-cache databases, made with 184 flushed commits inside `D3D12CreateDevice`, and the
/// driver's compiled pipelines) per user and per exe path, so the person's first launch takes its
/// window over to the GPU about 0.1 s after its first frame, as a later launch does, not after
/// 1 to 2.5 s with three or four threads busy beside their first keys. An update at the same path
/// keeps the cache. Nothing else of the app starts: no storage, no instance claim, no server.
#[cfg(windows)]
pub fn warm_gpu_if_asked() {
    if std::env::args().nth(1).as_deref() != Some("--warm-gpu") {
        return;
    }
    let code = match rune::warm_gpu_cache() {
        Ok(()) => 0,
        Err(why) => {
            eprintln!("opencode-rune: the GPU's cache was not made: {why}");
            1
        }
    };
    std::process::exit(code);
}
/// Windows: the app does not keep its own install folder as its working folder.
///
/// A process holds its working folder open, and so does every child that inherits it (the
/// server, the browser engine, a shell). Started with the install folder as the working folder
/// (a double-click on the exe of an unpacked build; a script), the app and the children that
/// outlive it kept that folder from being renamed, which is how an update swaps it: the update's
/// helper was refused (`ERROR_SHARING_VIOLATION`) for as long as one of them lived, 0.30, 0.55
/// and 4.55 s from the old app's exit to the swap for holds of 0.12, 0.3 and 4.3 s, and never
/// when the child stayed (the update then fails after its last try). The Start menu shortcut
/// starts the app in the home folder; here any start from inside the install folder goes there.
#[cfg(windows)]
pub fn leave_install_folder() {
    let (Ok(cwd), Ok(exe)) = (std::env::current_dir(), std::env::current_exe()) else { return };
    let Some(root) = exe.parent() else { return };
    let inside = |path: &std::path::Path| path.to_string_lossy().to_lowercase().starts_with(&root.to_string_lossy().to_lowercase());
    if !inside(&cwd) {
        return;
    }
    if let Some(home) = std::env::var_os("USERPROFILE").map(std::path::PathBuf::from).filter(|home| home.is_dir() && !inside(home)) {
        std::env::set_current_dir(home).ok();
    }
}
/// `opencode-desktop --version`: the version and the release channel it was built for.
#[cfg(not(target_arch = "wasm32"))]
pub fn print_version_if_asked() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        // A reader that went away (a GUI-subsystem exe captured by a shell that stopped waiting) is
        // no reason to panic.
        std::io::Write::write_fmt(
            &mut std::io::stdout(),
            format_args!("shuvcode-rune {} ({})\n", crate::updater::build_version(), crate::updater::build_channel()),
        )
        .ok();
        std::process::exit(0);
    }
}

/// `opencode-desktop --unwind-check`: panics 17 frames deep, each frame holding a destructor,
/// once under `catch_unwind` and once on a thread, and says whether both ended as panics do and
/// every destructor ran; exit 0 when they did. The packaged macOS build changes how its
/// functions' unwind entries are stored (`scripts/rustc-compact-unwind.sh`), and the app relies
/// on a panic staying where it is caught; `smoke-app.sh` runs this on every packaged build.
#[cfg(not(target_arch = "wasm32"))]
pub fn unwind_check_if_asked() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    if std::env::args().nth(1).as_deref() != Some("--unwind-check") {
        return;
    }
    static DROPPED: AtomicUsize = AtomicUsize::new(0);
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
    #[inline(never)]
    fn deep(level: usize) -> usize {
        let _guard = Guard;
        let below = if std::hint::black_box(level) == 0 { panic!("the unwind check's own panic") } else { deep(level - 1) };
        std::hint::black_box(below + 1)
    }
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(|| deep(16)).is_err();
    let alone = std::thread::spawn(|| deep(16)).join().is_err();
    std::panic::set_hook(hook);
    let dropped = DROPPED.load(Ordering::Relaxed);
    std::io::Write::write_fmt(
        &mut std::io::stdout(),
        format_args!(
            "unwind: a panic 17 frames deep was {}, a panicking thread {}, {dropped} of 34 destructors ran\n",
            if caught { "caught" } else { "not caught" },
            if alone { "ended alone" } else { "did not end as a panic" }
        ),
    )
    .ok();
    std::process::exit(if caught && alone && dropped == 34 { 0 } else { 1 });
}
/// Window title (`"OpenCode (Rune)"` on `main`, `"OpenCode (Rune) · <branch>"` on PR builds).
pub fn window_title() -> &'static str {
    // Stable and beta builds name themselves after their channel (`win_install::default_name`).
    #[cfg(not(target_arch = "wasm32"))]
    let fallback = win_install::default_name(option_env!("OPENCODE_RUNE_BUILD_CHANNEL"));
    #[cfg(target_arch = "wasm32")]
    let fallback = "Shuvcode (Rune)";
    option_env!("OPENCODE_RUNE_WINDOW_TITLE").unwrap_or(fallback)
}

/// The build's identity: its name, its app id (the Windows AppUserModelID, the uninstall
/// entry's key and the single-instance pipe's name) and its own URL scheme, from the same
/// environment the release workflows give every artifact of a build (the setup too).
#[cfg(not(target_arch = "wasm32"))]
pub fn product() -> win_install::Product {
    let product = win_install::Product::from_build(
        option_env!("OPENCODE_RUNE_WINDOW_TITLE"),
        option_env!("OPENCODE_RUNE_BUNDLE_ID"),
        option_env!("OPENCODE_RUNE_BUILD_CHANNEL"),
        option_env!("OPENCODE_RUNE_URL_SCHEME"),
        crate::updater::build_version(),
    );
    // `--app-id=<id>`, honoured only when `RUNE_DIRS_ROOT` moves every folder the app writes: a
    // harness run's own AppUserModelID, so its taskbar button, jump list and toasts are its own.
    match crate::dirs::rooted()
        .then(|| rune::extra_args().iter().find_map(|a| a.strip_prefix("--app-id=").map(str::to_owned)))
        .flatten()
    {
        Some(id) => product.with_id(id),
        None => product,
    }
}

/// What `rune::single_instance` keeps one process of: the data folder, as Desktop's
/// single-instance lock is its `userData` folder's. Every build (installed, another channel, a
/// release build from a checkout) shares that folder and its database, which the running process
/// holds locked, so a second one hands its links over rather than start without its state.
/// Debug builds have a folder of their own, and runs with their own `RUNE_DIRS_ROOT` their root.
#[cfg(not(target_arch = "wasm32"))]
pub fn instance_id() -> String {
    use sha2::{Digest, Sha256};
    let data = crate::dirs::data().unwrap_or_default();
    let digest = Sha256::digest(data.display().to_string().to_lowercase().as_bytes());
    let tag: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{tag}", product().family())
}

/// Windows: the windows' title bar, taskbar and Alt+Tab icon, from icon resource 1 of the module
/// this code is linked into: the one-file exe, or a packaged build's library (its `build.rs`).
#[cfg(target_os = "windows")]
pub fn apply_app_icon() {
    rune::set_app_icon_resource(1);
}

/// Sets the Dock icon to the embedded default during `cargo run`. A bundle names its own
/// (`CFBundleIconFile`: `Contents/Resources/icon.icns`, a PR build's with its badge), and the Dock
/// shows that without being asked: setting the same image again read and checked 1.4 MB in `main`
/// (11,100 µs) and had AppKit render it on the main thread before `applicationDidFinishLaunching`
/// (21,400 µs; a bare AppKit app with one window, M4: its window came 19,000 µs later).
/// A packaged macOS app (`cfg(bundle_assets)`) is always in its bundle and carries no second copy
/// of the icon in its exe (1.4 MiB).
///
/// Linux: the desktop finds the app's icon by the window's application id (Wayland `app_id`, X11
/// `WM_CLASS`), matched to `opencode-rune.desktop`, as Desktop's `StartupWMClass: appId` does.
#[cfg(all(not(target_os = "windows"), not(target_arch = "wasm32")))]
pub fn apply_app_icon() {
    #[cfg(target_os = "linux")]
    rune::set_app_id("shuvcode-rune");
    #[cfg(not(bundle_assets))]
    {
        let bundled = std::env::current_exe()
            .ok()
            .and_then(|exe| Some(exe.parent()?.parent()?.join("Resources/icon.icns")))
            .is_some_and(|icon| icon.is_file());
        if !bundled {
            rune::set_app_icon(include_bytes!("../../../../assets/icon.icns").as_slice()).ok();
        }
    }
}

/// Initial server target: `--server` if passed, otherwise `:4096` (plus `service.json` discovery
/// immediately when `on_ready` won't run, such as `--live --snap`).
pub fn initial_server() -> Server {
    let explicit = crate::arg("--server");
    // This connection is also cloned by restore, prefetch and reconnect. Negotiating media
    // references can initialize a server location and count it as a project visit. A shared
    // reader must therefore take ordinary message payloads, including for the initial page.
    let server = Server::new(&explicit.clone().unwrap_or_else(opencode_kit::client::default_server));
    if explicit.is_none() && !deferred() {
        opencode_kit::local_service::discover_local_server(&server)
    } else {
        server
    }
}

/// `main`, for runs that call `on_ready`: the server reads wait for it.
pub fn defer() {
    DEFERRED.store(true, Ordering::Relaxed);
}

/// Whether `App::load` leaves the server reads to `on_ready`.
pub fn deferred() -> bool {
    DEFERRED.load(Ordering::Relaxed)
}

/// What [`First::fetch`] reads; the projects' answer says whether the server is OpenCode's.
const FIRST_READS: [&str; 7] =
    ["/api/project", "/api/session?limit=120", "/api/model", "/api/agent", "/api/provider", "/api/session/active", "/api/config"];

/// The page, as it starts: the browser asks for the first reads while the module starts and
/// draws its first frame, and `on_ready`'s reads take up the answers.
#[cfg(target_arch = "wasm32")]
pub fn start_first_reads() {
    initial_server().start_gets(&FIRST_READS);
}

/// The first reads: projects, sessions, models and agents, providers, and what's running.
pub struct First {
    projects: Value,
    sessions: Option<Value>,
    models: Option<Value>,
    agents: Option<Value>,
    providers: Option<Value>,
    active: Option<Value>,
    /// The config's documents, for the model it names (`configured.ts`).
    config: Option<Value>,
}

impl First {
    pub fn fetch(server: &Server) -> Result<First, String> {
        let get = |path: &'static str| {
            let start = Instant::now();
            let r = server.get(path);
            crate::telemetry::launch_read(path, start);
            r
        };
        // A page started them with itself (`start_first_reads`); a retry starts them here.
        #[cfg(target_arch = "wasm32")]
        server.start_gets(&FIRST_READS);
        let [projects, paths @ ..] = FIRST_READS;
        // A page has no threads: its fiber takes up the answers one after another, but the
        // browser asked for them all at once.
        #[cfg(target_arch = "wasm32")]
        let (projects, [sessions, models, agents, providers, active, config]) = {
            let projects = get(projects).inspect_err(|_| server.forget_gets())?;
            (projects, paths.map(|path| get(path).ok()))
        };
        // All seven at once: each is its own round trip, and one after another they add up. The
        // projects' read was asked alone first and the other six after its answer: against a
        // server 300 ms away that was a round trip (300 ms) before the others started.
        #[cfg(not(target_arch = "wasm32"))]
        let (projects, [sessions, models, agents, providers, active, config]) = std::thread::scope(|scope| {
            let rest = paths.map(|path| scope.spawn(move || get(path).ok()));
            (get(projects), rest.map(|task| task.join().ok().flatten()))
        });
        #[cfg(not(target_arch = "wasm32"))]
        let projects = projects?;
        Ok(First { projects, sessions, models, agents, providers, active, config })
    }
}

/// What `on_ready`'s background startup worker brings back: Desktop Beta's tabs (when no
/// `tabs.json` exists yet) and the first server reads, or `first: None` when the server wasn't
/// reachable and the window should fall back to the fixture.
pub struct Ready {
    pub server: Option<Server>,
    pub beta_tabs: Option<Vec<crate::tabs::Tab>>,
    pub first: Option<Result<First, String>>,
    /// Nothing accepted a connection at the address, and the reach watcher tries it from now on
    /// (`Server::watch_reach`): the launch waits for it rather than trying on a timer.
    pub refused: bool,
    /// The restored session's newest page, asked for beside the first reads, for the session's
    /// reader to take up when the window connects (`Pages::live`).
    pub page: Option<session_pages::Early>,
}

/// The path of session `id`'s newest page, as its reader asks for it.
fn newest_page(id: &str) -> String {
    format!("/api/session/{id}/message?limit={}&order=desc", session_pages::LATEST)
}

/// Asks `server` for the restored session's newest page (`page`, its path) without waiting for
/// it. A page's reader asks for its own (`start_first_reads` asked for the rest as it started).
fn ask_page(server: &Server, page: Option<String>) -> Option<session_pages::Early> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (server, page);
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    page.and_then(|path| session_pages::Early::ask(server, path))
}

/// What the head start finds, as it finds it.
#[cfg(not(target_arch = "wasm32"))]
struct Head {
    /// The registered service's address once it took a connection: none when no service is
    /// registered or nothing accepts connections there.
    reach: Receiver<Option<String>>,
    /// Its first reads, when it took the connection.
    first: Receiver<Result<First, String>>,
}

#[cfg(not(target_arch = "wasm32"))]
static HEAD: std::sync::Mutex<Option<Head>> = std::sync::Mutex::new(None);

/// From the top of `main`, for a window that will find its server through `service.json`: the
/// registered service is asked for the first reads now, on a thread, while the app's state
/// loads, its plugins start and its window is made (about 19 ms of a warm launch). The launch's
/// first probe takes the answers up (`spawn_probe_waking`); nothing here needs the app's state.
/// The service's answers to them are the proof that it is OpenCode's: `/api/info` isn't asked
/// first, which was a round trip of its own.
#[cfg(not(target_arch = "wasm32"))]
pub fn head_start() {
    // As `App::load` decides a live window, and `spawn_probe_waking` that it discovers its server.
    if crate::arg("--server").is_some() || crate::arg("--fixture").is_some() || crate::demo::requested() {
        return;
    }
    let (reached, reach) = std::sync::mpsc::channel();
    let (read, first) = std::sync::mpsc::channel();
    let work = move || {
        let default = Server::new(&opencode_kit::client::default_server());
        let candidate = opencode_kit::local_service::registered(&default).filter(Server::reachable);
        reached.send(candidate.as_ref().map(|c| c.base.clone())).ok();
        let Some(candidate) = candidate else { return };
        REACHED.store(true, Ordering::Relaxed);
        rune::marks::mark("reads.head.asked");
        read.send(First::fetch(&candidate)).ok();
        rune::marks::mark("reads.head.in");
    };
    if std::thread::Builder::new().name("opencode-first-head".into()).spawn(work).is_ok() {
        *HEAD.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Head { reach, first });
    }
}

/// When the probe's thread found the server taking connections, set once from that thread.
pub type Accepted = std::sync::Arc<std::sync::OnceLock<Instant>>;

/// Whether a server took this launch's connection: the head start's, or a probe's.
static REACHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a server took a connection of this launch's (it may not have answered yet): what
/// the saved session list waits for before it stands in for the server's (`saved_list`).
pub(super) fn reached() -> bool {
    REACHED.load(Ordering::Relaxed)
}

/// How long the launch's first reads may wait on a server that took their connection before the
/// window says it's slow (`Link::Slow`): a server that accepts and holds every request would
/// otherwise say nothing until the reads time out (about 10 s).
pub const FIRST_PATIENCE: Duration = Duration::from_millis(1500);

/// How long a launch keeps trying a server it hasn't reached before Home shows the setup card:
/// a server that is just starting comes up in this time. Only the local default address waits:
/// another (a remote server, a tunnel) that refuses is unreachable at once, as Desktop's health
/// check settles on its first answer.
pub const GRACE: Duration = Duration::from_secs(5);

/// The server `server` stands for (`service.json` when `discover`), probed until it answers or
/// `grace` runs out; true when it answered. `registered_up`: what the caller has just found at
/// the registered service's address, for the first try.
fn probe(server: &Server, discover: bool, grace: Duration, mut registered_up: Option<bool>) -> (Server, bool) {
    let start = Instant::now();
    let mut pause = Duration::from_millis(250);
    loop {
        let tried = Instant::now();
        // Finding the server tried its address: it isn't tried again here.
        let (actual, up) = if discover { opencode_kit::local_service::discover_local_reach(server, registered_up.take()) } else { (server.clone(), None) };
        if up.unwrap_or_else(|| actual.reachable()) {
            return (actual, true);
        }
        let left = grace.saturating_sub(start.elapsed());
        if left.is_zero() {
            return (actual, false);
        }
        // A try that ran out its 250 ms (`Server::reachable`) waited with a SYN in flight, unanswered:
        // Windows answers a refused loopback SYN with silence, resending it, not a reset. Try
        // again at once, so a SYN is always out and a server that starts listening during the
        // grace is found within 250 ms, not after a back-off of up to a second more. A refusal at
        // once (a reset, as elsewhere) backs off. This only runs for the launch's 5 s grace.
        if tried.elapsed() >= Duration::from_millis(200) {
            continue;
        }
        std::thread::sleep(pause.min(left));
        pause = (pause * 2).min(Duration::from_secs(1));
    }
}

/// The probe, Desktop Beta's tabs and the first reads, on a thread; the window wakes for them.
pub(super) fn spawn_probe(s: &App, beta_tabs: bool, fallback_fixture: bool, grace: Duration, retry: bool) -> (Receiver<Ready>, Accepted) {
    spawn_probe_waking(s, beta_tabs, fallback_fixture, grace, retry, s.launch.inbox_wake.clone().or_else(|| (*s.waker).clone()))
}

/// [`spawn_probe`], waking through `wake`.
pub(super) fn spawn_probe_waking(s: &App, beta_tabs: bool, fallback_fixture: bool, grace: Duration, retry: bool, wake: Option<rune::Waker>) -> (Receiver<Ready>, Accepted) {
    let (out, rx) = std::sync::mpsc::channel();
    let accepted = Accepted::default();
    let Source::Live { server, .. } = &*s.source else { return (rx, accepted) };
    let took = accepted.clone();
    let server = server.clone();
    // Only the app's own server may stand down for the fixture, or move to the port
    // `service.json` names.
    let own = crate::servers::runtime::active(s).is_sidecar();
    let fallback = fallback_fixture && own;
    let discover_local = own && crate::arg("--server").is_none();
    let server = if discover_local {
        crate::servers::runtime::own_server().unwrap_or(server)
    } else {
        server
    };
    // A page whose first reads are all in (index.html asked as it started) takes them now, so the
    // next frame, the first one usually, is Home with its sessions: as task work it'd wait for the
    // page's next turn, after that frame.
    #[cfg(target_arch = "wasm32")]
    let now = server.gets_in(&FIRST_READS);
    #[cfg(not(target_arch = "wasm32"))]
    let now = false;
    // The session the window restores, when it's the app's own server's: its newest page is
    // read with the first reads, not a round trip after them.
    let page = match &*s.route {
        Route::Session { id, server } if server == crate::servers::runtime::active(s) && own => Some(newest_page(id)),
        _ => None,
    };
    // The launch's first probe takes up what `head_start` asked for from the top of `main`.
    #[cfg(not(target_arch = "wasm32"))]
    let head = if discover_local { HEAD.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() } else { None };
    let work = move || {
        // What the head start found at the registered service's address, for the probe below.
        #[cfg(not(target_arch = "wasm32"))]
        let mut registered_up = None;
        #[cfg(target_arch = "wasm32")]
        let registered_up = None;
        // The registered service's answers to the first reads say it is OpenCode's. One that
        // fails them, or isn't there, is found the long way below, as every later probe is.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(head) = head
            && let Ok(reach) = head.reach.recv()
        {
            registered_up = Some(reach.is_some());
            let candidate = opencode_kit::local_service::registered(&server).filter(|c| reach.as_deref() == Some(c.base.as_str()));
            if let Some(candidate) = candidate {
                took.set(Instant::now()).ok();
                if let Some(w) = &wake {
                    w();
                }
                // Beside the head start's reads, not after them: with a server 300 ms away the
                // page was a second round (asked at 333 ms, in at 645 ms).
                let early = ask_page(&candidate, page.clone());
                if let Ok(first @ Ok(_)) = head.first.recv() {
                    let beta_tabs = beta_tabs.then(crate::tabs::from_beta).flatten();
                    out.send(Ready { server: Some(candidate), beta_tabs, first: Some(first), refused: false, page: early }).ok();
                    if let Some(w) = wake {
                        w();
                    }
                    return;
                }
            }
        }
        // When managing the local background service (windowed run with no explicit --server /
        // OPENCODE_SERVER_URL), ensure `opencode serve --service` is running just like official
        // Desktop (`packages/desktop/src/main/service/background-service.ts`).
        #[cfg(not(target_arch = "wasm32"))]
        let sidecar_result = if discover_local && crate::servers::sidecar::managed() {
            Some(if retry { crate::servers::sidecar::reconnect() } else { crate::servers::sidecar::connection() })
        } else {
            None
        };
        #[cfg(target_arch = "wasm32")]
        let sidecar_result: Option<Result<Server, String>> = { let _ = retry; None };

        let (actual, reached, start_err) = match sidecar_result {
            Some(Ok(actual)) => (actual, true, None),
            Some(Err(err)) => {
                // Fall back to a single non-waiting check of `:4096` in case a plain server is up.
                let (actual, reached) = probe(&server, discover_local, Duration::ZERO, registered_up);
                (actual, reached, Some(err))
            }
            None => {
                let (actual, reached) = probe(&server, discover_local, grace, registered_up);
                (actual, reached, None)
            }
        };
        let ready = if !reached {
            if fallback {
                Ready { server: None, beta_tabs: None, first: None, refused: false, page: None }
            } else {
                let msg = start_err.unwrap_or_else(|| format!("No OpenCode server responded at {}", actual.base));
                // `service.json` may name another port next time, so a discovered server keeps
                // its timer.
                let refused = !discover_local;
                if refused {
                    actual.watch_reach(wake.clone());
                }
                Ready { server: Some(actual), beta_tabs: None, first: Some(Err(msg)), refused, page: None }
            }
        } else {
            // The window counts its patience from here (`poll`).
            REACHED.store(true, Ordering::Relaxed);
            took.set(Instant::now()).ok();
            if let Some(w) = &wake {
                w();
            }
            let beta_tabs = beta_tabs.then(crate::tabs::from_beta).flatten();
            let page = ask_page(&actual, page);
            let first = First::fetch(&actual);
            Ready { server: Some(actual), beta_tabs, first: Some(first), refused: false, page }
        };
        out.send(ready).ok();
        if let Some(w) = wake {
            w();
        }
    };
    if now {
        work();
    } else if let Err(e) = rune::task::spawn("opencode-first", work) {
        eprintln!("opencode-rune: failed to spawn first reads: {e}");
    }
    (rx, accepted)
}

impl App {
    /// Home shows server `key` (a server picked in Settings' switcher, a plugin's `Servers:Connect`):
    /// every server stays connected, and the window's tabs stay, as Desktop never switches a
    /// window to a server (`notes/multi-server.md` §5.1).
    pub(crate) fn focus_server(&mut self, key: &opencode_kit::servers::ServerKey, tx: &mut Tx) {
        if !self.live() {
            return;
        }
        tx.instant();
        crate::projects::home::focus(self, key);
        *self.opening = None;
        *self.route = Route::Home;
        crate::servers::runtime::switch(self, key);
    }

    /// Attachments and markdown images read through the server, so a window that opens with
    /// a transcript (a torn-out tab's) shows its images before it connects.
    pub(crate) fn set_media_fetch(&mut self) {
        let Source::Live { server, .. } = &*self.source else { return };
        let media = server.clone();
        self.composer_ext.attach.set_fetch(std::sync::Arc::new(move |hash: &str| media.media(hash).map_err(|e| e.to_string())));
        // Markdown images read through the server as Desktop's `readLocalImage` does; web
        // images and link favicons come from the web, as the browser fetches them for Desktop.
        let files = server.clone();
        #[cfg(not(target_arch = "wasm32"))]
        let home = (!self.home.is_empty()).then(|| (*self.home).clone());
        self.composer_ext.attach.markdown.set_fetch(
            std::sync::Arc::new(move |directory: &str, path: &str| {
                #[cfg(not(target_arch = "wasm32"))]
                if files.is_local() {
                    let root = opencode_feature_open_in::pinned_local_root(directory, home.as_deref())
                        .unwrap_or_else(|| directory.to_owned());
                    let abs = opencode_kit::path::absolute(&root, path);
                    if !browser_policy::verify_local_path(&abs).is_ok_and(|v| v.is_file) {
                        return Err("refused non-local or unsafe image path".to_owned());
                    }
                }
                let encoded = crate::review::data::encode_fs_path(path);
                files
                    .get_bytes(&format!("/api/fs/read/{encoded}?location%5Bdirectory%5D={}", crate::switcher::encode(directory)))
                    .map_err(|e| e.kind.to_string())
            }),
            std::sync::Arc::new(crate::web_fetch::get),
        );
    }

    pub(crate) fn apply_first(&mut self, f: First) {
        // A window that started in its opener's copy (a torn-out tab's) reads only what it
        // lacks: the copy is live, and newer than these reads.
        let joined = crate::windows::peer::joined(self);
        if !joined || self.projects.is_empty() {
            *self.projects = opencode_kit::model::parse_projects(&f.projects);
        }
        if let Some(sessions) = &f.sessions {
            // The saved list stood in for this one: a session the server no longer lists goes.
            if let Some(saved) = self.launch.saved_list.take() {
                saved.retire(&mut self.sessions);
            }
            self.refresh_recent();
            self.sessions_listed(opencode_kit::model::parse_sessions(sessions));
        }
        crate::connect::picker_reads::first(self, [f.providers, f.config, f.models, f.agents]);
        if let Some(a) = f.active.as_ref().filter(|_| !joined) {
            let map = a.get("data").unwrap_or(a);
            *self.running = map
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| k.clone())
                .filter(|id| !self.stops.pending(id))
                .collect();
            self.stops.active_sync.mark_boot_synced();
        }
        crate::windows::peer::touch(self);
        // A fresh profile has used no projects yet: Home lists none, as Desktop's
        // `usedProjectIDs` starts empty.
        let saved_projects = match &*self.source {
            Source::Live { server, .. } => Some(crate::tabs::load_projects(&server.base).unwrap_or_default()),
            Source::Fixture => None,
        };
        self.init_used_projects(saved_projects);
        if let Route::Session { id, server } = &*self.route
            && server == crate::servers::runtime::active(self)
            && !self.tabs.iter().any(|t| t.id == *id)
            && !self.sessions.iter().any(|s| s.id == *id)
        {
            self.launch.cold_transcript = None;
            *self.route = Route::Home;
        }
        self.sort_projects();
    }
}

/// With the data in: the event stream, info for tabs past the listed page, the open session,
/// `--session`, and the newest pages.
pub(super) fn connect(s: &mut App, tx: &mut Tx) {
    // Running sessions (subagents) whose family isn't loaded yet.
    let mut missing: Vec<String> = s.running.iter().filter_map(|id| s.missing_ancestor(id)).collect();
    missing.sort();
    missing.dedup();
    s.set_media_fetch();
    if let (Source::Live { server, requests, writes, .. }, Some(waker)) = (&mut *s.source, (*s.waker).clone()) {
        s.pages.set(session_pages::Pages::live(server.clone(), waker.clone(), s.launch.early_page.take()));
        let inbox_wake = s.launch.inbox_wake.clone().unwrap_or(waker);
        let (r, w, inbox) = crate::windows::connect(server, inbox_wake.clone());
        *writes = Some(w);
        // Tabs for sessions past the listed page: their info, so marks and avatars resolve.
        for t in s.tabs.iter().filter(|t| !t.is_draft() && !s.sessions.iter().any(|x| x.id == t.id)) {
            r.send(Request::Info(t.id.clone())).ok();
        }
        for id in missing {
            r.send(Request::Info(id)).ok();
        }
        *requests = Some(r);
        *s.inbox = Some(inbox);
        crate::windows::peer::join(s, Some(inbox_wake.clone()));
        // A window that started in its opener's copy was woken plainly until now.
        crate::windows::peer::set_waker(s, inbox_wake);
        // Home's provider tip waits for the default location's integrations (`home::tip`), which
        // are otherwise read only when Connect or Settings opens: with only the free models, read them now.
        if !s.composer_ext.pickers.models.iter().any(|m| m.provider != "opencode") {
            s.sync_catalogs(None);
        }
        super::onboarding::first_launch(s, tx);
        // The session open at last quit (see `restore`) subscribes like a click on its tab.
        if let Route::Session { id, server } = (*s.route).clone()
            && server == *crate::servers::runtime::active(s)
        {
            // The open session's own server has answered: with tabs on several servers it can be
            // later than `server.answered`, which is the first of them.
            rune::marks::mark("session.server.answered");
            crate::telemetry::open_restored(&id, s.transcripts.get(&id).map_or(0, |t| t.messages.len()));
            s.open(&id, tx);
        }
    }

    // `--session <id or title words>` opens a session of the app's own server at start; in a
    // browser, of the page's server (`?session=`, the address a session's page has).
    if let Some(raw) = arg("--session").filter(|_| cfg!(target_arch = "wasm32") || crate::servers::runtime::active(s).is_sidecar()) {
        let q = raw.to_lowercase();
        let found = s
            .sessions
            .iter()
            .find(|x| x.id == raw)
            .or_else(|| s.sessions.iter().find(|x| x.title.to_lowercase().contains(&q)))
            .map(|x| x.id.clone());
        if let Some(id) = found {
            s.open(&id, tx);
        }
    }
    // Every Git project's worktrees, one read each on the connection's one fetch thread, which
    // answers in the order asked: after the open session's own reads (its inbox, its subagents,
    // the permission or question it waits on), not before them. Asked first, 40 projects held
    // those back from 111 to 214 ms after the process started (a real server, each read 1-2 ms),
    // so a session waiting on a permission showed its dock that late.
    s.list_unlisted_worktrees();
    // What the server lists now is kept for the next launch's first frame, after this frame
    // and the next ones have drawn it (`launch::saved_write`).
    s.launch.saved_write = super::SavedWrite::Owed { at: *s.clock + super::SAVED_WRITE_AFTER.as_secs_f64() };
    tx.wake_after(super::SAVED_WRITE_AFTER);
    // Small newest pages, on the speculative lane after foreground startup work is queued.
    let mut recent: Vec<_> = s.sessions.iter().filter(|s| s.parent.is_none()).collect();
    recent.sort_by_key(|s| std::cmp::Reverse(s.updated));
    for session in recent.into_iter().take(session_pages::WARM) {
        s.prefetch_page(&session.id);
    }
    // Deterministic headless snapshots have no wall-clock event loop to await the worker, so
    // they read the open session inline. A `--realtime` probe stands in for a window and has
    // one: `open` above already asked for both on the fetch thread, and a read here held its
    // frame for a whole round trip, twice (5 and 10 s behind a server that answers in 5 s),
    // which no window does.
    let inline = headless() && !crate::realtime();
    if inline
        && let (Route::Session { id, server: on }, Source::Live { server, .. }) = (&*s.route, &*s.source)
        && on == crate::servers::runtime::active(s)
        && !s.transcripts.contains_key(id)
        && let Ok(v) = server.read(&format!("/api/session/{id}/message?limit={}&order=desc", session_pages::LATEST))
    {
        s.transcripts.insert(id.clone(), Transcript { session: id.clone(), messages: opencode_kit::model::parse_messages(&v) });
    }
    // And what waits in its inbox: steers, and shells and subagents that came back.
    if inline
        && let (Route::Session { id, server: on }, Source::Live { server, .. }) = (&*s.route, &*s.source)
        && on == crate::servers::runtime::active(s)
        && let Ok(v) = server.read(&format!("/api/session/{id}/inbox"))
    {
        let id = id.clone();
        let items = v["data"].as_array().or_else(|| v.as_array()).cloned().unwrap_or_default();
        s.inbox_listed(&id, &items);
        if let Some(dir) = s.session(&id).map(|i| i.directory.clone()) {
            crate::subagents::shells::list(s, &dir);
        }
        crate::subagents::jobs::list(s, &id);
    }
}

