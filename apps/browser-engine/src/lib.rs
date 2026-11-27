//! Standalone `OpenCode Browser Engine` CEF 156 sidecar daemon.
//!
//! Listens on a private Unix domain socket (AF_UNIX on Windows too), streams GPU frames (2x
//! Retina `IOSurface`s over Mach ports on macOS, D3D11 shared textures on Windows), survives UI
//! hot-reloads (15s reconnect grace window), and exits cleanly after 60s with 0 tabs.
//!
//! The main thread sleeps until something happens: a message from the app, a connection, or
//! the time CEF asked to do its work at (`external_message_pump`). Nothing polls.

mod context;
#[cfg(windows)]
mod d3d_frames;
mod downloads;
#[cfg(target_os = "linux")]
mod dmabuf_frames;
mod engine;
mod extensions;
pub mod helper;
mod output;
mod permissions;
mod handlers;
mod keys;
#[cfg(target_os = "linux")]
mod linux_gpu;
#[cfg(target_os = "macos")]
mod mac_app;
mod agent;
mod cdp_ledger;
mod cef_runtime;
mod clear_storage;
mod clipboard_gate;
mod cookies;
mod diagnostics;
mod favicon;
mod frame_authority;
mod find;
mod input_fps;
mod input_guard;
mod media_gate;
mod notification_gate;
mod menu;
mod menu_extras;
#[cfg(windows)]
mod modal_pump;
mod passkeys;
mod preview;
mod profile;
mod prompts;
mod tab_client;
mod wire;

use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(windows)]
use uds_windows::{UnixListener, UnixStream};

use crate::engine::{CefRuntime, EngineManager};
use crate::wire::HostMsg;

/// How long the engine waits for its app to come back after it disconnected without a Shutdown.
const RECONNECT_GRACE: Duration = Duration::from_secs(15);
/// How long the engine stays up with no tab open, so a page opened again soon starts at once.
const EMPTY_GRACE: Duration = Duration::from_secs(60);
/// The longest the loop sleeps between CEF's work calls while it's idle, in case a scheduling
/// call is missed.
#[cfg(not(target_os = "macos"))]
const MAX_PUMP_DELAY: Duration = Duration::from_secs(1);
/// ...and for `BUSY_FOR` after the app asks for something: Chromium doesn't announce all of a new
/// page's work (cefclient pumps every 33 ms for that reason, always), and with only the 1 s cap
/// a new tab's first frame came 1.3-2.4 s after it opened instead of ~0.4 s.
#[cfg(not(target_os = "macos"))]
const BUSY_PUMP_DELAY: Duration = Duration::from_millis(16);
#[cfg(not(target_os = "macos"))]
const BUSY_FOR: Duration = Duration::from_secs(3);
/// macOS pumps the AppKit run loop every 4 ms while a tab is awake, relaxing to 50 ms when idle
/// (0 tabs or all tabs hidden) to avoid 250 Hz idle wakeups.
#[cfg(target_os = "macos")]
const MAX_PUMP_DELAY: Duration = Duration::from_millis(4);
#[cfg(target_os = "macos")]
const IDLE_PUMP_DELAY: Duration = Duration::from_millis(50);
#[cfg(target_os = "macos")]
const BUSY_PUMP_DELAY: Duration = MAX_PUMP_DELAY;
#[cfg(target_os = "macos")]
const BUSY_FOR: Duration = Duration::ZERO;
/// Windows: while a tab waits for the app to free a frame texture, how often it looks (Linux
/// repaints when the app's read arrives).
const FRAME_RETRY: Duration = Duration::from_millis(2);

/// What wakes the main thread.
pub enum Event {
    /// Connection `id`, and the half the engine writes to; its messages follow as `Host`.
    Connected { id: u64, writer: UnixStream },
    Disconnected { id: u64 },
    Host(Box<HostMsg>),
    /// CEF wants `cef::do_message_loop_work` in this many milliseconds (now when 0 or less).
    Pump(i64),
    /// Background parallel SHA-256 tree verification finished for startup `--extension` specs.
    ExtensionsVerified(extensions::integrity::StartupExtensions),
    /// Main-frame metadata update for a non-isolated tab (feeds extension `tabs`/`webNavigation`).
    TabMeta {
        tab_id: u64,
        url: Option<String>,
        title: Option<String>,
        loading: Option<bool>,
    },
    /// Frame-level navigation signal for `chrome.webNavigation` (main frame + subframes).
    WebNav(extensions::web_nav::WebNavSignal),
    /// Linux: a tab's frames can't come the way they do.
    #[cfg(target_os = "linux")]
    FramesFailed(dmabuf_frames::Failure),
}

/// Whether something the engine waits for is there, and since when it isn't.
#[derive(Clone, Copy)]
enum Presence {
    Present,
    AbsentSince(Instant),
}

impl Presence {
    fn step(self, present: bool, now: Instant) -> Self {
        match (self, present) {
            (_, true) => Presence::Present,
            (Presence::Present, false) => Presence::AbsentSince(now),
            (absent, false) => absent,
        }
    }

    fn deadline(self, grace: Duration) -> Option<Instant> {
        match self {
            Presence::Present => None,
            Presence::AbsentSince(since) => Some(since + grace),
        }
    }
}

/// Windows CEF bootstrap entry point (`include/cef_sandbox_win.h::RunWinMain`): invoked by
/// CEF's `bootstrap.exe` (renamed to `opencode-browser-engine.exe`) for both the browser process
/// and every sandboxed subprocess (renderer, GPU, utility). Subprocesses MUST execute and return
/// from `cef::execute_process` immediately without touching files, sockets, pipes, or D3D11.
#[cfg(windows)]
#[allow(unsafe_code, non_snake_case)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn RunWinMain(
    h_instance: *mut std::ffi::c_void,
    _lp_cmd_line: *mut u16,
    _n_cmd_show: i32,
    sandbox_info: *mut std::ffi::c_void,
    _version_info: *mut std::ffi::c_void,
) -> i32 {
    assert!(
        !sandbox_info.is_null(),
        "Windows browser engine RunWinMain requires non-null sandbox_info from CEF bootstrap.exe"
    );
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let main_args = cef::MainArgs {
        instance: cef::sys::HINSTANCE(h_instance.cast()),
    };
    let mut helper_app = helper::new_helper_app();
    let exit_code = cef::execute_process(Some(&main_args), Some(&mut helper_app), sandbox_info.cast());
    if exit_code >= 0 {
        return exit_code;
    }
    run_browser(sandbox_info, Some(main_args))
}

pub fn run_browser(
    sandbox_info: *mut std::ffi::c_void,
    main_args_override: Option<cef::MainArgs>,
) -> i32 {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--print-cef-dir") {
        if let Some(dir) = option_env!("OPENCODE_CEF_DIR") {
            println!("{dir}");
        } else if let Some(dir) = cef_dll_sys::get_cef_dir() {
            println!("{}", dir.display());
        }
        return 0;
    }
    // Debug builds only: which profile and cookie key a start would use, without starting CEF (so
    // a proof never reaches the Keychain).
    #[cfg(target_os = "macos")]
    if cfg!(debug_assertions) && args.iter().any(|a| a == "--print-profile-decision") {
        let profile = profile::Profile::open();
        println!(
            "key={} lasting={} root={}",
            mac_app::cookie_key().name(),
            !profile.temporary,
            profile.root.display()
        );
        return 0;
    }
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));
    // `--trace-startup` (or OPENCODE_BROWSER_ENGINE_TRACE in the environment, which reaches an
    // engine the app starts): each start step's µs since main and its wall time, on stderr.
    let entered = Instant::now();
    let trace = args.iter().any(|a| a == "--trace-startup") || std::env::var_os("OPENCODE_BROWSER_ENGINE_TRACE").is_some();
    let step = |name: &str| {
        if trace {
            let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            eprintln!("engine-startup {name} {:.1} unix {:.6}", entered.elapsed().as_secs_f64() * 1e6, wall.as_secs_f64());
        }
    };
    step("main");

    #[cfg(windows)]
    end_children_with_engine();
    #[cfg(windows)]
    let mut high_res_timer = HighResTimer::new();
    let ext_specs: Vec<String> = args
        .windows(2)
        .filter(|w| w[0] == "--extension")
        .map(|w| w[1].clone())
        .collect();
    // Unix: reserve FDs 3 and 4 BEFORE `UnixListener::bind` so the IPC socket lands on FD >= 5
    // and is never clobbered when `CdpPipe::create` places Chromium's pipe ends on FDs 3 and 4.
    if !ext_specs.is_empty() {
        extensions::pipe::reserve_unix_fds();
    }
    let sock_path = arg("--socket").map(PathBuf::from).unwrap_or_else(default_socket_path);
    if let Some(parent) = sock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path).expect("bind browser-engine unix socket");
    // The app waits for this line instead of retrying its connect: from here it connects at once,
    // and its first messages wait in the socket while CEF starts.
    {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"listening\n");
        let _ = out.flush();
    }
    step("listening");

    // Lossless unbounded control event channel (`TabMeta`, `Pump`, `FramesFailed`, `Host` control
    // messages are never dropped and never wait behind preview stream bytes).
    let (events, rx) = mpsc::channel::<Event>();
    let previews = preview::PreviewRegistry::new();
    let profile = profile::Profile::open();
    // Chromium's temporary files (and its helpers', which inherit this) go in the profile, not
    // the user's temp folder: everything the engine writes is under the root the app gave it.
    let temp = profile.root.join("Temp");
    let _ = std::fs::remove_dir_all(&temp);
    let _ = std::fs::create_dir_all(&temp);
    // SAFETY: still single-threaded before any worker or CEF thread is spawned.
    unsafe {
        std::env::set_var("TMP", &temp);
        std::env::set_var("TEMP", &temp);
    }

    // Fast (<1 ms) pre-CEF preparation: validate only MV3 manifest + key->id and write 100-byte
    // inert exemption stubs (0 perms, 0 scripts, 0 DNR) so `--disable-extensions-except` locks
    // Chromium's exemption set without adding ~1.7 s to `cef::initialize()`.
    let prepared = extensions::integrity::prepare_startup_stubs(&ext_specs, &profile.root);
    let mut pre_rejected = prepared.pre_rejected_ids;
    let mut stub_root = prepared.stub_root;
    let mut stub_lock = prepared.stub_lock;
    let mut stub_dirs = prepared.stub_dirs;
    let mut stub_dirs_by_id = prepared.stub_dirs_by_id;
    let mut candidates = prepared.candidates;

    // Create the private in-process anonymous CDP pipe BEFORE spawning any threads or
    // `cef::initialize` (single-threaded on Unix so `dup2` + `FD_CLOEXEC` has zero fork race).
    // Fail closed to `--disable-extensions` if pipe creation fails (`review-rd274.md` §7).
    let ext_pipe_bundle = if !stub_dirs.is_empty() {
        let created = extensions::pipe::CdpPipe::create(events.clone());
        if created.is_none() {
            for cand in std::mem::take(&mut candidates) {
                pre_rejected.push(cand.id);
            }
            stub_dirs.clear();
            stub_dirs_by_id.clear();
            drop(stub_lock.take());
            if let Some(root) = stub_root.take() {
                extensions::integrity::cleanup_stub_root(&root);
            }
            extensions::pipe::release_reserved_unix_fds();
        }
        created
    } else {
        if !ext_specs.is_empty() {
            extensions::pipe::release_reserved_unix_fds();
        }
        None
    };
    let io_pipes_switch = ext_pipe_bundle
        .as_ref()
        .and_then(|(p, _, _)| p.io_pipes_switch().map(str::to_owned));

    // Full parallel hardware-accelerated SHA-256 tree verification runs concurrently with
    // `cef::initialize()` (~35-60 ms hidden completely behind CEF's ~135 ms init).
    if !candidates.is_empty() {
        let verify_tx = events.clone();
        let verify_stub_root = stub_root.clone();
        let _ = std::thread::Builder::new()
            .name("engine-ext-verify".into())
            .spawn(move || {
                let res = extensions::integrity::verify_candidates_parallel(
                    candidates,
                    verify_stub_root.as_deref(),
                );
                let _ = verify_tx.send(Event::ExtensionsVerified(res));
            });
    }
    let client_writer: Arc<Mutex<Option<(u64, Arc<UnixStream>)>>> = Arc::default();
    accept_connections(listener, events.clone(), Arc::clone(&previews));

    // The app's GPU (`HIGH,LOW` LUID halves): Chromium renders on it, so its frames open there.
    let adapter_luid = arg("--adapter-luid")
        .and_then(|v| v.split_once(','))
        .and_then(|(high, low)| Some((high.parse::<i32>().ok()?, low.parse::<u32>().ok()?)));
    // The app's language as Chromium names it (`de`, `zh-CN`): page strings and Accept-Language.
    let locale = arg("--lang").cloned();
    let _cpu_frames = args.iter().any(|a| a == "--cpu-frames");
    // Windows: the D3D11 device frames are copied on (tens of ms) is made beside CEF's start.
    #[cfg(windows)]
    let gpu = std::thread::Builder::new()
        .name("engine-gpu".into())
        .spawn(move || (!_cpu_frames).then(|| d3d_frames::Gpu::new(adapter_luid)).flatten())
        .expect("spawn engine-gpu");
    // Linux: the app's render node when it imports dmabufs; the EGL context frames are copied
    // with is made beside CEF's start too.
    #[cfg(target_os = "linux")]
    let gpu = {
        let node = linux_gpu::plan(arg("--dmabuf-device").cloned());
        std::thread::Builder::new()
            .name("engine-gpu".into())
            .spawn(move || node.map(|node| dmabuf_frames::Gpu::new(&node)))
            .expect("spawn engine-gpu")
    };
    let _cef = CefRuntime::initialize(
        profile,
        adapter_luid,
        locale.as_deref(),
        &stub_dirs,
        stub_root.clone(),
        stub_lock,
        io_pipes_switch,
        events.clone(),
        sandbox_info,
        main_args_override,
    );
    context::lockdown_global_native_messaging();
    let menu_storage_dir = _cef.download_roots().profile.join("opencode-ext-menus");
    let ext_host = ext_pipe_bundle.map(|(pipe, rx_in, wake)| {
        pipe.mark_adopted_by_cef();
        extensions::ExtensionHost::new(
            pipe,
            rx_in,
            wake,
            stub_dirs_by_id,
            stub_root.clone(),
            Some(menu_storage_dir),
        )
    });
    step("cef_initialized");
    #[cfg(windows)]
    modal_pump::install();

    let (out_tx, out_rx) = output::Output::channel();
    {
        let writer_slot = client_writer.clone();
        std::thread::Builder::new()
            .name("engine-sock-tx".into())
            .spawn(move || {
                for (msg, fds) in out_rx {
                    let Some((id, stream)) = writer_slot.lock().unwrap().clone() else { continue };
                    let Ok(mut line) = serde_json::to_vec(&msg) else { continue };
                    line.push(b'\n');
                    #[cfg(unix)]
                    let written = output::write_with_fds(&stream, &line, &fds);
                    #[cfg(not(unix))]
                    let written = {
                        let _ = fds;
                        let mut writer = stream.as_ref();
                        writer.write_all(&line)
                    };
                    if written.is_err() {
                        let mut guard = writer_slot.lock().unwrap();
                        if guard.as_ref().is_some_and(|(current, _)| *current == id) {
                            *guard = None;
                        }
                    }
                }
            })
            .expect("spawn engine-sock-tx");
    }

    #[cfg(windows)]
    let mut manager = EngineManager::new(
        out_tx,
        _cef.download_roots(),
        events.clone(),
        previews,
        gpu.join().ok().flatten(),
        ext_host,
        pre_rejected,
    );
    #[cfg(target_os = "linux")]
    let mut manager = EngineManager::new(
        out_tx,
        _cef.download_roots(),
        events.clone(),
        previews,
        linux_gpu::transport(gpu.join().ok().flatten()),
        ext_host,
        pre_rejected,
    );
    #[cfg(target_os = "macos")]
    let mut manager = EngineManager::new(
        out_tx,
        _cef.download_roots(),
        events.clone(),
        previews,
        ext_host,
        pre_rejected,
    );
    step("manager");
    let mut first_message = true;
    let start = Instant::now();
    let (mut client, mut tabs) = (Presence::AbsentSince(start), Presence::AbsentSince(start));
    let mut pump_at = Some(start);
    let mut last_pump = start;
    // Until when the app's last request keeps CEF's work calls close together.
    let mut busy_until = start + BUSY_FOR;

    // One event: false when the engine should stop.
    macro_rules! dispatch {
        ($event:expr) => {
            match $event {
                Event::Connected { id, writer } => {
                    if let Some((_, old)) = client_writer.lock().unwrap().replace((id, Arc::new(writer))) {
                        let _ = old.shutdown(std::net::Shutdown::Both);
                    }
                    true
                }
                Event::Disconnected { id } => {
                    let disconnected_current = {
                        let mut writer = client_writer.lock().unwrap();
                        if writer.as_ref().is_some_and(|(current, _)| *current == id) {
                            *writer = None;
                            true
                        } else {
                            false
                        }
                    };
                    if disconnected_current {
                        manager.on_host_disconnected();
                    }
                    true
                }
                Event::Host(msg) => {
                    let now = Instant::now();
                    busy_until = now + BUSY_FOR;
                    if matches!(
                        msg.as_ref(),
                        HostMsg::Wheel { .. }
                            | HostMsg::MouseMove { .. }
                            | HostMsg::MouseButton { .. }
                            | HostMsg::Key { .. }
                    ) {
                        pump_at = Some(now);
                    }
                    if std::mem::take(&mut first_message) {
                        step("first_message");
                    }
                    manager.handle(*msg)
                }
                #[cfg(target_os = "linux")]
                Event::FramesFailed(failure) => {
                    manager.frames_failed(failure);
                    true
                }
                Event::Pump(delay_ms) => {
                    let at = Instant::now() + Duration::from_millis(delay_ms.max(0) as u64);
                    pump_at = Some(pump_at.map_or(at, |current| current.min(at)));
                    true
                }
                Event::ExtensionsVerified(startup) => {
                    step("extensions_verified");
                    busy_until = Instant::now() + BUSY_FOR;
                    pump_at = Some(Instant::now());
                    manager.on_extensions_verified(startup);
                    true
                }
                Event::TabMeta { tab_id, url, title, loading } => {
                    manager.on_tab_meta(tab_id, url, title, loading);
                    true
                }
                Event::WebNav(sig) => {
                    manager.on_web_navigation(sig);
                    true
                }
            }
        };
    }

    'run: loop {
        // What already waits goes before CEF's work, so the app's first messages (Hello, the tab
        // it opens) don't queue behind it; at most a turn's worth, so CEF's work keeps its turn.
        for _ in 0..64 {
            match rx.try_recv() {
                Ok(event) => {
                    if !dispatch!(event) {
                        break 'run;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break 'run,
            }
        }
        if manager.poll_extensions() {
            busy_until = Instant::now() + BUSY_FOR;
            pump_at = Some(Instant::now());
        }
        let now = Instant::now();
        #[cfg(target_os = "macos")]
        let max_pump = if !manager.has_awake_tabs() {
            IDLE_PUMP_DELAY
        } else {
            MAX_PUMP_DELAY
        };
        #[cfg(not(target_os = "macos"))]
        let max_pump = MAX_PUMP_DELAY;
        let cap = if now < busy_until { BUSY_PUMP_DELAY } else { max_pump };
        if pump_at.is_some_and(|at| at <= now) || now.saturating_duration_since(last_pump) >= cap {
            pump_at = None;
            last_pump = now;
            #[cfg(not(windows))]
            cef::do_message_loop_work();
            // A person's print dialog keeps this call until it closes: meanwhile its loop runs
            // the engine's turn (`modal_pump`), so the app's messages and the other tabs go on.
            #[cfg(windows)]
            {
                let mut stop = false;
                modal_pump::work(&mut || {
                    while let (false, Ok(event)) = (stop, rx.try_recv()) {
                        stop = !dispatch!(event);
                    }
                    cef::do_message_loop_work();
                    manager.poll_picks();
                    manager.release_passkey_held();
                    manager.poll_frames();
                });
                if stop {
                    break 'run;
                }
            }
            manager.poll_picks();
            manager.release_passkey_held();
        }
        #[cfg(target_os = "macos")]
        mac_app::pump_run_loop(0.004);
        manager.poll_frames();

        let connected = client_writer.lock().unwrap().is_some();
        client = client.step(connected, now);
        tabs = tabs.step(!manager.is_empty(), now);
        let quit_at = [client.deadline(RECONNECT_GRACE), tabs.deadline(EMPTY_GRACE)].into_iter().flatten().min();
        if quit_at.is_some_and(|at| at <= now) {
            break;
        }
        let wake = [Some(pump_at.unwrap_or(now + cap)), quit_at, manager.waits_for_texture().then(|| now + FRAME_RETRY)]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(now + max_pump);
        #[cfg(windows)]
        high_res_timer.sync(
            now < busy_until
                || manager.waits_for_texture()
                || pump_at.is_some_and(|at| at.saturating_duration_since(now) < Duration::from_millis(50)),
        );
        let event = match rx.recv_timeout(wake.saturating_duration_since(now)) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => {
                if pump_at.is_none() {
                    pump_at = Some(Instant::now());
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if !dispatch!(event) {
            break;
        }
    }
    manager.shutdown_extensions();
    let _ = std::fs::remove_file(&sock_path);
    0
}

/// Accepts the app's connections on a thread of its own (the last one is the app) and starts
/// reading each at once, so its first messages wait in the queue while CEF starts.
fn accept_connections(
    listener: UnixListener,
    events: Sender<Event>,
    previews: Arc<preview::PreviewRegistry>,
) {
    std::thread::Builder::new()
        .name("engine-sock-accept".into())
        .spawn(move || {
            for (id, stream) in (1u64..).zip(listener.incoming()) {
                let Ok(stream) = stream else { continue };
                let Ok(writer) = stream.try_clone() else { continue };
                if events.send(Event::Connected { id, writer }).is_err() {
                    break;
                }
                read_messages(id, stream, events.clone(), Arc::clone(&previews));
            }
        })
        .expect("spawn engine-sock-accept");
}

/// Reads connection `id`'s messages on a thread of its own until it closes.
/// Routes preview data frames (`PreviewResponse{Head,Chunk,End}`) directly into `previews` so
/// streaming preview bytes NEVER enter the lossless control `events` channel.
fn read_messages(
    id: u64,
    stream: UnixStream,
    events: Sender<Event>,
    previews: Arc<preview::PreviewRegistry>,
) {
    let _ = std::thread::Builder::new().name("engine-sock-rx".into()).spawn(move || {
        // Hard cap on one NDJSON command line from the host (32 MiB accommodates large cookie
        // import batches and base64 preview chunks while stopping unbounded allocation).
        const MAX_WIRE_LINE_BYTES: usize = 32 * 1024 * 1024;
        let mut reader = BufReader::new(stream);
        let mut buf: Vec<u8> = Vec::new();
        'outer: loop {
            buf.clear();
            let mut oversized = false;
            loop {
                let Ok(avail) = reader.fill_buf() else { break 'outer };
                if avail.is_empty() {
                    if buf.is_empty() {
                        break 'outer;
                    }
                    break;
                }
                if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
                    if !oversized && buf.len() + pos <= MAX_WIRE_LINE_BYTES {
                        buf.extend_from_slice(&avail[..pos]);
                    } else {
                        oversized = true;
                    }
                    reader.consume(pos + 1);
                    break;
                }
                let n = avail.len();
                if !oversized && buf.len() + n <= MAX_WIRE_LINE_BYTES {
                    buf.extend_from_slice(avail);
                } else {
                    oversized = true;
                    buf.clear();
                }
                reader.consume(n);
            }
            if oversized || buf.is_empty() {
                continue;
            }
            let Ok(line) = std::str::from_utf8(&buf) else { continue };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<HostMsg>(line) else { continue };
            match msg {
                HostMsg::PreviewResponseHead { req_id, ok, status, mime, content_length, content_range } => {
                    previews.deliver_head(req_id, ok, status, &mime, content_length, content_range);
                }
                HostMsg::PreviewResponseChunk { req_id, data_b64 } => {
                    previews.deliver_chunk(req_id, &data_b64);
                }
                HostMsg::PreviewResponseEnd { req_id, ok } => {
                    previews.deliver_end(req_id, ok);
                }
                other => {
                    if events.send(Event::Host(Box::new(other))).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = events.send(Event::Disconnected { id });
    });
}

/// Puts the engine in a job that ends every process in it when the engine's last handle to it
/// goes, that is when the engine exits however it exits: CEF's renderers, GPU and utility
/// processes never outlive it.
#[cfg(windows)]
#[allow(unsafe_code)]
fn end_children_with_engine() {
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain Win32 calls on a job this process owns; the handle is kept open on purpose
    // (closing it is what ends the job's processes), so it's never closed here.
    unsafe {
        let Ok(job) = CreateJobObjectW(None, windows::core::PCWSTR::null()) else { return };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(job, JobObjectExtendedLimitInformation, (&raw const limits).cast(), size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32);
        if set.is_ok() {
            let _ = AssignProcessToJobObject(job, GetCurrentProcess());
        }
    }
}

#[cfg(target_os = "linux")]
fn default_socket_path() -> PathBuf {
    profile::xdg_dir("XDG_CACHE_HOME", ".cache").join("browser.sock")
}

#[cfg(target_os = "macos")]
fn default_socket_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let uid = unsafe { libc::geteuid() };
    PathBuf::from(home)
        .join("Library/Caches/co.anomaly.opencode")
        .join(format!("browser-{uid}.sock"))
}

/// Explicit state machine scoping Windows' 1 ms timer period (`timeBeginPeriod(1)`) to active
/// interaction, frame retries, and short pump intervals so idle tabs / laptops do not burn 1 kHz
/// timer interrupts. On startup it opts this windowless sidecar out of Windows 11's
/// `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION` so active 1 ms requests are honored.
#[cfg(windows)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum HighResTimerState {
    #[default]
    Inactive,
    Active,
}

#[cfg(windows)]
enum HighResTimerEffect {
    Begin,
    End,
}

#[cfg(windows)]
impl HighResTimerState {
    fn step(&mut self, want_active: bool) -> Option<HighResTimerEffect> {
        match (*self, want_active) {
            (Self::Inactive, true) => {
                *self = Self::Active;
                Some(HighResTimerEffect::Begin)
            }
            (Self::Active, false) => {
                *self = Self::Inactive;
                Some(HighResTimerEffect::End)
            }
            _ => None,
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
struct HighResTimer(HighResTimerState);

#[cfg(windows)]
#[allow(unsafe_code)]
impl HighResTimer {
    fn new() -> Self {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
            ProcessPowerThrottling, SetProcessInformation,
        };
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        // SAFETY: plain Win32 call on this process handle and an owned `state` struct.
        unsafe {
            let _ = SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&raw const state).cast(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
        Self(HighResTimerState::Inactive)
    }

    fn sync(&mut self, want_active: bool) {
        match self.0.step(want_active) {
            Some(HighResTimerEffect::Begin) => {
                // SAFETY: paired with `timeEndPeriod(1)` on transition to `Inactive` or `Drop`.
                if unsafe { windows::Win32::Media::timeBeginPeriod(1) } != 0 {
                    self.0 = HighResTimerState::Inactive;
                }
            }
            Some(HighResTimerEffect::End) => {
                // SAFETY: paired with the prior `timeBeginPeriod(1)` that entered `Active`.
                unsafe {
                    let _ = windows::Win32::Media::timeEndPeriod(1);
                }
            }
            None => {}
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
impl Drop for HighResTimer {
    fn drop(&mut self) {
        self.sync(false);
    }
}

/// The app always passes `--socket`; this is for running the engine by hand.
#[cfg(windows)]
fn default_socket_path() -> PathBuf {
    std::env::temp_dir().join("co.anomaly.opencode").join("browser.sock")
}
