//! The engine's socket (Unix domain, AF_UNIX on Windows too), launch and framing threads.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use rune::time::Instant;
use rune::Waker;

use crate::engine_log;
use crate::frame::{self, TabSurface};
use crate::installer::{self, EngineStatus, Fault, Installed};
use crate::machine::{ConnectOrigin, EarlyOpensMap, EngineEffect, EngineEvent, SharedEngine};
use crate::profile;
use crate::protocol::{EngineMsg, HostMsg};

#[cfg(unix)]
pub(crate) use std::os::unix::net::UnixStream;
#[cfg(windows)]
pub(crate) use uds_windows::UnixStream;

/// Each tab's surface by tab id, which frames are pushed to off the UI thread.
pub(crate) type Surfaces = Arc<Mutex<HashMap<u64, TabSurface>>>;

/// How long a new engine may take to open its socket: the first start after an install can take
/// several seconds while the OS scans the freshly unpacked files (Windows Defender does, for
/// each new DLL).
const LISTEN_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a freshly attached engine may take to finish `CefRuntime::initialize()` and send
/// its first `EngineMsg::Ready`. Chromium startup runs after the engine prints `"listening"` and
/// can take longer than `ANSWER_DEADLINE` on cold boot, after an update (Windows Defender scan),
/// or under heavy load.
const CONNECT_DEADLINE: Duration = Duration::from_secs(30);

/// How long an already-ready engine may take to answer a request (a page to load, a tab to open,
/// a DevTools call) before the pane says it isn't responding. Any message at all is an answer: a
/// page that loads slowly still says at once that it's loading.
const ANSWER_DEADLINE: Duration = Duration::from_secs(5);

#[cfg(windows)]
static SPAWN_SOCK_SEQ: AtomicU64 = AtomicU64::new(1);

/// What the app shares with the threads that talk to its engine.
#[derive(Clone)]
pub(crate) struct Link {
    pub generation: u64,
    pub shared: Arc<Mutex<SharedEngine>>,
    pub inbox_tx: Sender<EngineMsg>,
    pub surfaces: Surfaces,
    pub waker: Option<Waker>,
    /// Its socket, removed once it's gone, so the next connect starts a new engine instead of
    /// reaching the old one while Windows still ends it.
    pub sock: PathBuf,
    /// Profile directory holding `debug.log`, `engine-host.log`, and `engine-stderr.log`.
    pub profile: Option<PathBuf>,
    /// In-memory tail of this spawned engine's stderr (populated on spawn; `None` for reused sockets).
    pub stderr: Option<engine_log::StderrTail>,
    /// The tabs that waited when the connect began, by id: sent the moment the engine is
    /// connected, behind Hello, without waiting for the app's next turn (one that's busy drawing
    /// held them for 20-650 ms); stored into `Connected::early` on attach so that turn doesn't
    /// send them again and corrects what changed meanwhile.
    pub opens: Vec<(u64, HostMsg)>,
    /// Verified `<id>:<sha256>:<dir>` specs passed as `--extension` arguments when spawning.
    pub extension_args: Vec<String>,
    pub profile_permissions: Vec<crate::prompts::SiteDecision>,
    pub preview_chunk_budget: Arc<crate::preview::PreviewChunkBudget>,
    #[cfg(target_os = "linux")]
    pub dmabuf_pools: Arc<Mutex<Option<Arc<Mutex<crate::dmabuf::Pools>>>>>,
}

impl Link {
    pub(crate) fn current(&self) -> bool {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner).is_current(self.generation)
    }

    pub(crate) fn set(&self, status: EngineStatus) {
        let effect = self.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::LinkStatus {
            generation: self.generation,
            status,
        });
        if matches!(effect, EngineEffect::Wake) {
            self.wake();
        }
    }

    fn mark_hung(&self) {
        let effect = self.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::Hung {
            generation: self.generation,
        });
        if matches!(effect, EngineEffect::Wake) {
            self.wake();
        }
    }

    fn recover_hung(&self) {
        let effect = self.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::Recovered {
            generation: self.generation,
        });
        if matches!(effect, EngineEffect::Wake) {
            self.wake();
        }
    }

    fn wake(&self) {
        if let Some(w) = &self.waker {
            w();
        }
    }
}

/// The engine at `bin`, serving `sock` and keeping its sign-ins in `profile`, in the app's
/// language. On Windows it renders on the GPU Rune draws this app's windows with, so its frames
/// open there.
pub(crate) fn engine_command(bin: &Path, sock: &Path, profile: &Path, extension_args: &[String]) -> Command {
    let mut cmd = Command::new(bin);
    cmd.arg("--socket").arg(sock).arg("--lang").arg(chromium_locale(opencode_i18n::locale().id())).env(profile::PROFILE_ENV, profile);
    for spec in extension_args {
        cmd.arg("--extension").arg(spec);
    }
    // Pipe stderr to a bounded host drain thread (or inherit when tracing) so panics, bind errors,
    // and Chromium fatal messages are preserved without leaking a writable file handle to children.
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(engine_log::stderr_stdio());
    #[cfg(windows)]
    if let Some((high, low)) = rune::native_surface::windows::adapter_luid() {
        cmd.arg("--adapter-luid").arg(format!("{high},{low}"));
    }
    // Linux: the GPU the window draws on, whose Vulkan device imports the engine's dmabufs.
    #[cfg(target_os = "linux")]
    if let Some(node) = crate::dmabuf::device() {
        cmd.arg("--dmabuf-device").arg(node);
    }
    cmd
}

/// Chromium's name for the app's language id, where they differ (package-windows.ps1 ships the
/// locales by the same names).
fn chromium_locale(id: &str) -> &str {
    match id {
        "en" => "en-US",
        "zh" => "zh-CN",
        "zht" => "zh-TW",
        "no" => "nb",
        other => other,
    }
}

/// Prepares the socket path for a freshly spawned engine. On Unix, `unlink` frees `base`
/// immediately; on Windows, a dying engine can hold `base` in kernel teardown (`DELETE_PENDING`
/// or sharing violation), so we sweep removable numeric sibling sockets we created and pick a
/// fresh sibling path so `UnixListener::bind` never panics.
#[cfg(not(windows))]
fn prepare_spawn_socket(base: &Path, _profile: &Path) -> PathBuf {
    std::fs::remove_file(base).ok();
    base.to_owned()
}

#[cfg(windows)]
fn prepare_spawn_socket(base: &Path, profile: &Path) -> PathBuf {
    std::fs::remove_file(base).ok();
    if matches!(std::fs::symlink_metadata(base), Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
        return base.to_owned();
    }
    let Some(parent) = base.parent() else {
        return base.to_owned();
    };
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("browser");
    // Preserve the unique trailing `exe_tag` / hash component when a compact name is needed.
    let short_stem = match stem.rsplit_once('-') {
        Some((_, tag)) if !tag.is_empty() => format!("b-{tag}"),
        _ => stem.chars().take(10).collect(),
    };
    let full_prefix = format!("{stem}-");
    let short_prefix = format!("{short_stem}-");
    let is_numeric_sibling = |name: &str| {
        let Some(without_ext) = name.strip_suffix(".sock") else { return false };
        let rest = without_ext
            .strip_prefix(&full_prefix)
            .or_else(|| without_ext.strip_prefix(&short_prefix));
        rest.is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    };
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_str().is_some_and(is_numeric_sibling) {
                let path = entry.path();
                if std::fs::symlink_metadata(&path)
                    .is_ok_and(|m| !m.is_dir() && !m.file_type().is_symlink() && m.len() == 0)
                {
                    std::fs::remove_file(path).ok();
                }
            }
        }
    }
    const MAX_BYTES: usize = 107;
    for _ in 0..16 {
        let seq = SPAWN_SOCK_SEQ.fetch_add(1, Ordering::Relaxed);
        let mut candidate = parent.join(format!("{stem}-{seq}.sock"));
        if candidate.as_os_str().len() > MAX_BYTES {
            candidate = parent.join(format!("{short_stem}-{seq}.sock"));
        }
        if candidate.as_os_str().len() <= MAX_BYTES
            && matches!(std::fs::symlink_metadata(&candidate), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        {
            engine_log::log(
                Some(profile),
                &format!("socket busy ({}), using fallback {}", base.display(), candidate.display()),
            );
            return candidate;
        }
    }
    base.to_owned()
}

/// Starts the engine and connects to it as soon as it listens (it says so on its output), on
/// the calling thread.
pub(crate) fn spawn_and_attach(engine: &Installed, sock: &Path, profile: &Path, mach_service: Option<String>, link: &Link) {
    if !link.current() {
        return;
    }
    // This app has no engine running (its socket didn't answer), so a held profile is an
    // engine on its way out, most likely the shared one from before per-app engines.
    profile::wait_until_free(profile, profile::UNLOCK_WAIT);
    if !link.current() {
        return;
    }
    let spawn_sock = prepare_spawn_socket(sock, profile);
    let mut link = link.clone();
    link.sock = spawn_sock.clone();
    crate::prompts::scrub_chromium_preferences_for_older_engine(profile, &link.profile_permissions);
    #[cfg(windows)]
    if let Some(dir) = engine.bin.parent()
        && let Err(e) = crate::unpack::grant_sandbox_rx(dir)
    {
        let msg = format!("sandbox permissions: {e}");
        engine_log::log(Some(profile), &format!("gen={} launch-failed {msg}", link.generation));
        return link.set(EngineStatus::Failed(Fault::Launch(msg).into()));
    }
    let started = Instant::now();
    engine_log::log(
        Some(profile),
        &format!("gen={} spawn bin={} sock={}", link.generation, engine.bin.display(), spawn_sock.display()),
    );
    let mut child = match engine_command(&engine.bin, &spawn_sock, profile, &link.extension_args).spawn() {
        Ok(child) => child,
        Err(e) => {
            let fault = spawn_fault(&e);
            engine_log::log(Some(profile), &format!("gen={} spawn-err fault={fault} os_err={e}", link.generation));
            if matches!(fault, Fault::Damaged(_)) {
                discard(engine);
            }
            return link.set(EngineStatus::Failed(fault.into()));
        }
    };
    if let Some(stderr) = child.stderr.take() {
        link.stderr = Some(engine_log::capture_stderr(stderr, profile.to_path_buf()));
    }
    let Some(stdout) = child.stdout.take() else {
        engine_log::log(Some(profile), &format!("gen={} missing stdout pipe", link.generation));
        return link.set(EngineStatus::Failed(Fault::Exited(None).into()));
    };
    let (said, heard) = mpsc::channel();
    if std::thread::Builder::new()
        .name("opencode-browser-listen".into())
        .spawn(move || {
            let mut line = String::new();
            said.send(BufReader::new(stdout).read_line(&mut line).is_ok_and(|n| n > 0) && line.trim() == "listening").ok();
        })
        .is_err()
    {
        child.kill().ok();
        engine_log::log(Some(profile), &format!("gen={} listen-thread-spawn-failed", link.generation));
        return link.set(EngineStatus::Failed(Fault::Launch("no thread to start it on".to_owned()).into()));
    }
    match heard.recv_timeout(LISTEN_TIMEOUT) {
        Ok(true) => {
            engine_log::log(
                Some(profile),
                &format!("gen={} pid={} listening after {}ms", link.generation, child.id(), started.elapsed().as_millis()),
            );
        }
        // It ended before it listened: its exit code says whether its files are damaged.
        Ok(false) => {
            let fault = exit_fault(&mut child);
            let tail = link.stderr.as_ref().and_then(engine_log::StderrTail::snapshot).unwrap_or_default();
            engine_log::log(
                Some(profile),
                &format!(
                    "gen={} pid={} exited-before-listen after {}ms fault={fault}{}",
                    link.generation,
                    child.id(),
                    started.elapsed().as_millis(),
                    if tail.is_empty() { String::new() } else { format!(" stderr={tail}") },
                ),
            );
            std::fs::remove_file(&spawn_sock).ok();
            if matches!(fault, Fault::Damaged(_)) {
                discard(engine);
            }
            return link.set(EngineStatus::Failed(fault.into()));
        }
        // It didn't listen in time: Retry ends it and starts a new one.
        Err(_) => {
            let tail = link.stderr.as_ref().and_then(engine_log::StderrTail::snapshot).unwrap_or_default();
            engine_log::log(
                Some(profile),
                &format!(
                    "gen={} pid={} listen-timeout ({}s){}",
                    link.generation,
                    child.id(),
                    LISTEN_TIMEOUT.as_secs(),
                    if tail.is_empty() { String::new() } else { format!(" stderr={tail}") },
                ),
            );
            child.kill().ok();
            std::fs::remove_file(&spawn_sock).ok();
            return link.set(EngineStatus::Failed(Fault::Hung.into()));
        }
    }
    rune::marks::mark("browser.engine.listening");
    if !link.current() {
        return;
    }
    match UnixStream::connect(&spawn_sock) {
        Ok(stream) => match wire_stream(stream, mach_service, &link, ConnectOrigin::Spawned) {
            WireResult::Attached => {}
            // Do not kill `child`: on `Stale` a newer generation may already be using it on the
            // same socket, and on `Failed` we cannot rule out a newer generation attaching once
            // `lost` released `shared`. An unused engine exits on its own after `RECONNECT_GRACE`.
            WireResult::Stale | WireResult::Failed => return,
        },
        Err(e) => {
            engine_log::log(
                Some(profile),
                &format!("gen={} pid={} connect-failed sock={} err={e}", link.generation, child.id(), spawn_sock.display()),
            );
            if link.current() {
                child.kill().ok();
                std::fs::remove_file(&spawn_sock).ok();
            }
            return link.set(EngineStatus::Failed(Fault::Stopped.into()));
        }
    }
    let pid = child.id();
    link.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::ProcessSpawned {
        generation: link.generation,
        pid,
    });
    std::thread::Builder::new()
        .name("opencode-browser-watch".into())
        .spawn(move || {
            let code = wait_for_end(&mut child);
            link.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::ProcessExited {
                generation: link.generation,
                pid,
            });
            lost(&link, code);
            drop(child);
        })
        .ok();
}

/// Why the OS refused to start the engine: its files are damaged (the program is gone or isn't
/// one), security software or a policy stopped it, or something else, in the OS's words.
fn spawn_fault(err: &std::io::Error) -> Fault {
    #[cfg(windows)]
    match err.raw_os_error() {
        // ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_BAD_EXE_FORMAT, ERROR_EXE_MACHINE_TYPE_MISMATCH.
        Some(code @ (2 | 3 | 193 | 216)) => return Fault::Damaged(code as u32),
        // ERROR_ACCESS_DENIED, ERROR_VIRUS_INFECTED, ERROR_VIRUS_DELETED, ERROR_ACCESS_DISABLED_NO_SAFER_UI_BY_POLICY,
        // ERROR_ACCESS_DISABLED_BY_POLICY, ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION (Smart App Control, App Control).
        Some(5 | 225 | 226 | 786 | 1260 | 4551) => return Fault::Prevented(err.to_string()),
        _ => {}
    }
    #[cfg(unix)]
    if matches!(err.kind(), std::io::ErrorKind::NotFound) {
        return Fault::Damaged(err.raw_os_error().unwrap_or_default() as u32);
    }
    Fault::Launch(err.to_string())
}

/// The engine closed its output before it listened: it's ending (Windows can take a moment to
/// report it). The Windows loader's exit codes mean files of the install are missing or damaged.
fn exit_fault(child: &mut Child) -> Fault {
    let code = (0..40)
        .find_map(|_| match child.try_wait() {
            Ok(Some(status)) => Some(status.code()),
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(50));
                None
            }
            Err(_) => Some(None),
        })
        .flatten()
        .map(|c| c as u32);
    if code.is_none() {
        child.kill().ok();
    }
    if let Some(code) = code {
        eprintln!("browser engine: ended while starting with exit code {code:#010x}");
    }
    match code {
        Some(c) if out_of_memory(c) => Fault::OutOfMemory,
        // STATUS_INVALID_IMAGE_FORMAT, STATUS_DLL_NOT_FOUND, STATUS_ENTRYPOINT_NOT_FOUND.
        Some(c @ (0xC000_007B | 0xC000_0135 | 0xC000_0139)) if cfg!(windows) => Fault::Damaged(c),
        // STATUS_INVALID_IMAGE_HASH: code integrity refused one of its files.
        Some(0xC000_0428) if cfg!(windows) => Fault::Prevented(format!("exit code {:#010x}", 0xC000_0428u32)),
        code => Fault::Exited(code),
    }
}

/// Removes a downloaded engine whose files are damaged, so Retry downloads it again (an engine
/// named by the environment is left alone).
fn discard(engine: &Installed) {
    if let (Some(version), Some(root)) = (engine.version, installer::try_engines_root()) {
        installer::discard(&root, version);
    }
}

/// Blocks until the engine has stopped running. On Windows that's its main thread's end: a
/// killed or crashed engine's main thread ends within ~20 ms, while its process, its exit code
/// and its socket wait until Windows has finished tearing it down (12-60 s measured on a Windows dev box,
/// with one of its threads still held in the kernel), and the engine's work all runs on its main
/// thread, whose end is the process's. Like the process, the thread is a handle to wait on: no
/// timer, no wake until it ends.
fn wait_for_end(child: &mut Child) -> Option<u32> {
    #[cfg(windows)]
    if let Some(thread) = proc::main_thread(child.id()) {
        return proc::wait(thread);
    }
    child.wait().ok()?.code().map(|c| c as u32)
}

/// Ends the engine process with `pid` at once (Retry, when it stopped answering). The watcher
/// still holds the engine's own handle, so its pid is still the engine's.
pub(crate) fn kill_pid(pid: u32) {
    proc::kill(pid);
}

#[allow(unsafe_code)]
mod proc {
    #[cfg(windows)]
    use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};

    /// The engine's main thread: of its threads, the one created first (with the process).
    #[cfg(windows)]
    pub(super) fn main_thread(pid: u32) -> Option<HANDLE> {
        use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next};
        use windows::Win32::System::Threading::{GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION, THREAD_SYNCHRONIZE};
        // SAFETY: the snapshot and every thread handle but the one returned are closed here;
        // `entry` and the times are ours to fill.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).ok()?;
            let mut entry = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
            let mut first: Option<(u64, HANDLE)> = None;
            let mut more = Thread32First(snap, &mut entry).is_ok();
            while more {
                if entry.th32OwnerProcessID == pid
                    && let Ok(thread) = OpenThread(THREAD_SYNCHRONIZE | THREAD_QUERY_LIMITED_INFORMATION, false, entry.th32ThreadID)
                {
                    let (mut created, mut a, mut b, mut c) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
                    let at = GetThreadTimes(thread, &mut created, &mut a, &mut b, &mut c).map(|_| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime));
                    match (at, first) {
                        (Ok(at), Some((best, old))) if at < best => {
                            CloseHandle(old).ok();
                            first = Some((at, thread));
                        }
                        (Ok(at), None) => first = Some((at, thread)),
                        _ => {
                            CloseHandle(thread).ok();
                        }
                    }
                }
                more = Thread32Next(snap, &mut entry).is_ok();
            }
            CloseHandle(snap).ok();
            first.map(|(_, thread)| thread)
        }
    }

    #[cfg(windows)]
    /// Waits for the thread's end and returns its exit code (the process's, for its main thread).
    pub(super) fn wait(thread: HANDLE) -> Option<u32> {
        use windows::Win32::System::Threading::{GetExitCodeThread, INFINITE, WaitForSingleObject};
        // SAFETY: `thread` is ours until it's closed here; `code` is ours to fill.
        unsafe {
            WaitForSingleObject(thread, INFINITE);
            let mut code = 0u32;
            let got = GetExitCodeThread(thread, &mut code).is_ok();
            CloseHandle(thread).ok();
            got.then_some(code)
        }
    }

    /// Whether commit memory is short now: under 512 MB left on the computer, or this process's
    /// job has reached its commit limit.
    #[cfg(windows)]
    pub(super) fn commit_short() -> bool {
        use windows::Win32::System::JobObjects::{JOB_OBJECT_LIMIT_JOB_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, QueryInformationJobObject};
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: both structures are ours to fill, with their sizes given.
        unsafe {
            let mut status = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
            if GlobalMemoryStatusEx(&mut status).is_ok() && status.ullAvailPageFile < 512 << 20 {
                return true;
            }
            let mut job = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
            QueryInformationJobObject(None, JobObjectExtendedLimitInformation, (&raw mut job).cast(), size, None).is_ok()
                && job.BasicLimitInformation.LimitFlags.0 & JOB_OBJECT_LIMIT_JOB_MEMORY.0 != 0
                && job.PeakJobMemoryUsed.saturating_add(64 << 20) >= job.JobMemoryLimit
        }
    }

    #[cfg(windows)]
    pub(super) fn kill(pid: u32) {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
        // SAFETY: the handle is closed before returning.
        unsafe {
            if let Ok(process) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                TerminateProcess(process, 1).ok();
                CloseHandle(process).ok();
            }
        }
    }

    #[cfg(unix)]
    pub(super) fn kill(pid: u32) {
        if let Ok(pid) = libc::pid_t::try_from(pid)
            && pid > 0
        {
            // SAFETY: `pid > 0` targets a single process (never 0 or a negative process group) and
            // `libc::kill` has no memory preconditions.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}
/// The engine went away on its own (the app's `shutdown_sidecar` transitions out of `Connected`
/// first): the pane says so, and Retry starts it again. The socket's end usually comes first,
/// without the process's exit code; the process's end then names the cause when it was a lack of
/// memory. Returns whether the transition applied for `link`'s generation.
fn lost(link: &Link, code: Option<u32>) -> bool {
    if !link.current() {
        return false;
    }
    let fault = if code.is_some_and(out_of_memory) { Fault::OutOfMemory } else { Fault::Stopped };
    let effect = link.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::Lost {
        generation: link.generation,
        fault: fault.clone(),
    });
    if !matches!(effect, EngineEffect::IdleLost | EngineEffect::RetryFresh)
        && let Some(code) = code
    {
        eprintln!("browser engine: ended with exit code {code:#010x}");
    }
    match effect {
        EngineEffect::RetryFresh => {
            engine_log::log(
                link.profile.as_deref(),
                &format!("gen={} retry-fresh (reused socket closed before ready)", link.generation),
            );
            std::fs::remove_file(&link.sock).ok();
            link.wake();
            true
        }
        EngineEffect::IdleLost => {
            engine_log::log(link.profile.as_deref(), &format!("gen={} idle-exit", link.generation));
            std::fs::remove_file(&link.sock).ok();
            link.wake();
            true
        }
        EngineEffect::LostFirst => {
            let tail = link.stderr.as_ref().and_then(engine_log::StderrTail::snapshot).unwrap_or_default();
            engine_log::log(
                link.profile.as_deref(),
                &format!(
                    "gen={} lost fault={fault} code={}{}",
                    link.generation,
                    code.map_or_else(|| "none".to_owned(), |c| format!("{c:#010x}")),
                    if tail.is_empty() { String::new() } else { format!(" stderr={tail}") },
                ),
            );
            rune::marks::mark("browser.engine.lost");
            std::fs::remove_file(&link.sock).ok();
            link.wake();
            true
        }
        EngineEffect::Wake => {
            engine_log::log(
                link.profile.as_deref(),
                &format!(
                    "gen={} lost-update fault={fault} code={}",
                    link.generation,
                    code.map_or_else(|| "none".to_owned(), |c| format!("{c:#010x}")),
                ),
            );
            link.wake();
            true
        }
        _ => false,
    }
}

/// Whether a process that ended with `code` couldn't get memory: STATUS_NO_MEMORY,
/// STATUS_COMMITMENT_LIMIT and Chromium's out-of-memory exception say so; a stack overflow (a stack
/// that couldn't grow) and a fail-fast (Rust's abort on a failed allocation) say so only while
/// commit is short.
fn out_of_memory(code: u32) -> bool {
    #[cfg(windows)]
    return matches!(code, 0xC000_0017 | 0xC000_012D | 0xE000_0008) || (matches!(code, 0xC000_00FD | 0xC000_0409) && proc::commit_short());
    #[cfg(not(windows))]
    {
        let _ = code;
        false
    }
}

/// Whether the engine answers `msg` with a message of its own soon: a page it loads says it's
/// loading, a tab it opens reports itself, a DevTools call returns.
fn expects_answer(msg: &HostMsg) -> bool {
    matches!(
        msg,
        HostMsg::Hello { .. }
            | HostMsg::OpenTab { .. }
            | HostMsg::Navigate { .. }
            | HostMsg::Reload { .. }
            | HostMsg::CdpCall { .. }
            | HostMsg::SiteCookies { .. }
            | HostMsg::ClearSite { .. }
            | HostMsg::ListExtensions { .. }
            | HostMsg::UninstallExtension { .. }
            | HostMsg::ReloadExtension { .. }
            | HostMsg::ClickExtension { .. }
    )
}

/// Pending-answer deadline state for one sidecar socket connection.
#[derive(Clone, Copy, Default)]
enum AskState {
    /// Socket attached; `Hello` has not yet been written. The first request uses `CONNECT_DEADLINE`
    /// so `CefRuntime::initialize()` gets the full startup budget instead of the 5 s command limit.
    #[default]
    BeforeHello,
    /// Waiting for the engine's first message (`Ready`) since `since` (`CONNECT_DEADLINE`).
    Connecting { since: Instant },
    /// Engine has answered at least once and has no pending request.
    Idle,
    /// Waiting for any engine message since `since` (`ANSWER_DEADLINE`).
    Waiting { since: Instant },
    /// Deadline passed and `SharedEngine` moved to `LiveStage::Hung`.
    Hung,
    /// Connection ended; watcher thread exits.
    Over,
}

enum AskEvent {
    Asked(Instant),
    Heard,
    DeadlinePassed,
    Ended,
}

enum AskEffect {
    None,
    Notify,
    RecoverAndNotify,
    MarkHung { startup: bool },
}

impl AskState {
    fn step(&mut self, event: AskEvent) -> AskEffect {
        match (*self, event) {
            (Self::BeforeHello, AskEvent::Asked(since)) => {
                *self = Self::Connecting { since };
                AskEffect::Notify
            }
            (Self::Connecting { .. }, AskEvent::Heard) | (Self::BeforeHello, AskEvent::Heard) => {
                *self = Self::Idle;
                AskEffect::Notify
            }
            (Self::Connecting { .. }, AskEvent::DeadlinePassed) => {
                *self = Self::Hung;
                AskEffect::MarkHung { startup: true }
            }
            (Self::Idle, AskEvent::Asked(since)) => {
                *self = Self::Waiting { since };
                AskEffect::Notify
            }
            (Self::Waiting { .. }, AskEvent::Heard) => {
                *self = Self::Idle;
                AskEffect::Notify
            }
            (Self::Hung, AskEvent::Heard) => {
                *self = Self::Idle;
                AskEffect::RecoverAndNotify
            }
            (Self::Waiting { .. }, AskEvent::DeadlinePassed) => {
                *self = Self::Hung;
                AskEffect::MarkHung { startup: false }
            }
            (_, AskEvent::Ended) => {
                *self = Self::Over;
                AskEffect::Notify
            }
            _ => AskEffect::None,
        }
    }
}

#[derive(Default)]
struct Answers {
    state: Mutex<AskState>,
    changed: Condvar,
}

impl Answers {
    fn ask(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(state.step(AskEvent::Asked(Instant::now())), AskEffect::Notify) {
            self.changed.notify_all();
        }
    }

    /// Any message answers; one after the pane said the engine isn't responding takes that back.
    fn heard(&self, link: &Link) {
        let effect = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let effect = state.step(AskEvent::Heard);
            if matches!(effect, AskEffect::Notify | AskEffect::RecoverAndNotify) {
                self.changed.notify_all();
            }
            effect
        };
        // Release `Answers::state` before disk I/O or acquiring `SharedEngine`.
        if matches!(effect, AskEffect::RecoverAndNotify) {
            engine_log::log(link.profile.as_deref(), &format!("gen={} recovered-from-hung", link.generation));
            link.recover_hung();
        }
    }

    fn end(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.step(AskEvent::Ended);
        self.changed.notify_all();
    }

    /// Sleeps until a request is asked, then until it's answered or its deadline passes: the
    /// thread only runs while the app waits on the engine.
    fn watch(&self, link: &Link) {
        loop {
            let hung = {
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                loop {
                    let (since, deadline) = match *state {
                        AskState::Over => return,
                        AskState::BeforeHello | AskState::Idle | AskState::Hung => {
                            state = self.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
                            continue;
                        }
                        AskState::Connecting { since } => (since, CONNECT_DEADLINE),
                        AskState::Waiting { since } => (since, ANSWER_DEADLINE),
                    };
                    let left = deadline.saturating_sub(since.elapsed());
                    if !left.is_zero() {
                        state = self.changed.wait_timeout(state, left).unwrap_or_else(PoisonError::into_inner).0;
                        continue;
                    }
                    if let AskEffect::MarkHung { startup } = state.step(AskEvent::DeadlinePassed) {
                        break (startup, deadline);
                    }
                }
            };
            // Release `Answers::state` before disk I/O or acquiring `SharedEngine`.
            let (startup, deadline) = hung;
            let tail = link.stderr.as_ref().and_then(engine_log::StderrTail::snapshot).unwrap_or_default();
            engine_log::log(
                link.profile.as_deref(),
                &format!(
                    "gen={} hung stage={} deadline={}s{}",
                    link.generation,
                    if startup { "connecting" } else { "ready" },
                    deadline.as_secs(),
                    if tail.is_empty() { String::new() } else { format!(" stderr={tail}") },
                ),
            );
            rune::marks::mark("browser.engine.hung");
            link.mark_hung();
        }
    }
}

/// Outcome of attaching a connected `UnixStream` to [`SharedEngine`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WireResult {
    /// Attached to the current generation and started the framing threads.
    Attached,
    /// `link.generation` was superseded before attach; caller must not touch the socket or child.
    Stale,
    /// `stream.try_clone()` failed for the current generation; reported via `lost` so Retry works.
    Failed,
}

pub(crate) fn wire_stream(stream: UnixStream, mach_service: Option<String>, link: &Link, origin: ConnectOrigin) -> WireResult {
    let Ok(reader_stream) = stream.try_clone() else {
        drop(stream);
        return if lost(link, None) { WireResult::Failed } else { WireResult::Stale };
    };
    let (tx, rx) = mpsc::channel::<HostMsg>();
    tx.send(HostMsg::Hello {
        mach_service,
        pid: std::process::id(),
        prompts: true,
        extensions: true,
        download_save: true,
        tab_lifecycle: true,
    })
    .ok();
    let mut early = EarlyOpensMap::new();
    for (id, open) in &link.opens {
        let is_agent = matches!(open, HostMsg::OpenTab { agent_driven: true, .. });
        if let HostMsg::OpenTab { url, width_pt, height_pt, scale, .. } = open {
            early.insert(*id, (url.clone(), (*width_pt, *height_pt, *scale)));
        }
        tx.send(open.clone()).ok();
        if is_agent {
            tx.send(HostMsg::SetAgentDriven { tab_id: *id, agent_driven: true }).ok();
        }
    }
    // Linux: frame reads and failures go to this connection only, never to a later engine's.
    #[cfg(target_os = "linux")]
    let frames_tx = tx.clone();
    let attached = link.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::Attach {
        generation: link.generation,
        origin,
        sock: link.sock.clone(),
        outbox: tx,
        early,
    });
    if matches!(attached, EngineEffect::Rejected) {
        return WireResult::Stale;
    }
    // Connected: the app's next turn sends the tabs that wait (`flush_pending_opens`).
    link.wake();

    let answers = Arc::new(Answers::default());
    let (watched, watcher) = (answers.clone(), link.clone());
    std::thread::Builder::new().name("rune-browser-answers".into()).spawn(move || watched.watch(&watcher)).ok();

    let asked = answers.clone();
    let chunk_budget = link.preview_chunk_budget.clone();
    std::thread::Builder::new()
        .name("rune-browser-tx".into())
        .spawn(move || {
            let mut writer = stream;
            while let Ok(msg) = rx.recv() {
                let is_shutdown = matches!(msg, HostMsg::Shutdown);
                if let Ok(mut line) = serde_json::to_string(&msg) {
                    line.push('\n');
                    if writer.write_all(line.as_bytes()).is_err() {
                        break;
                    }
                    writer.flush().ok();
                    if expects_answer(&msg) {
                        asked.ask();
                    }
                }
                if is_shutdown {
                    break;
                }
            }
            // Release any waiting preview workers when the engine disconnects or shuts down.
            chunk_budget.reset();
            // The app let the engine go (or it went): nothing more is waited for.
            asked.end();
        })
        .ok();

    let link = link.clone();
    std::thread::Builder::new()
        .name("rune-browser-rx".into())
        .spawn(move || {
            #[cfg(target_os = "linux")]
            let mut reader = crate::fd_lines::FdLines::new(reader_stream);
            #[cfg(not(target_os = "linux"))]
            let mut reader = BufReader::new(reader_stream);
            #[cfg(target_os = "linux")]
            let (pools, mut frames) = (
                Arc::new(Mutex::new(crate::dmabuf::Pools::new(frames_tx))),
                crate::dmabuf::Frames::None,
            );
            #[cfg(target_os = "linux")]
            {
                *link.dmabuf_pools.lock().unwrap_or_else(PoisonError::into_inner) = Some(pools.clone());
            }
            let mut line = String::new();
            #[cfg(not(target_os = "linux"))]
            let mut raw_buf: Vec<u8> = Vec::new();
            loop {
                let shrink_line = line.len() <= 64 * 1024 && line.capacity() > 256 * 1024;
                line.clear();
                if shrink_line {
                    line.shrink_to(64 * 1024);
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let shrink_raw = raw_buf.len() <= 64 * 1024 && raw_buf.capacity() > 256 * 1024;
                    raw_buf.clear();
                    if shrink_raw {
                        raw_buf.shrink_to(64 * 1024);
                    }
                }
                #[cfg(target_os = "linux")]
                let Ok((1.., line_at)) = reader.read_line(&mut line) else { break };
                #[cfg(target_os = "linux")]
                if !link.current() { break; }
                #[cfg(not(target_os = "linux"))]
                {
                    match read_bounded_wire_line(&mut reader, &mut raw_buf, &mut line) {
                        BoundedLine::Line => {
                            if !link.current() { break; }
                        }
                        BoundedLine::Oversized => {
                            if !link.current() { break; }
                            continue;
                        }
                        BoundedLine::Eof => break,
                    }
                }
                // The sidecar writes the type tag first. Borrow the large base64 field directly
                // from the reusable line buffer. Other layouts still go through the wire enum.
                if line.starts_with("{\"type\":\"cpu_frame\",")
                    && let Ok(paint) = serde_json::from_str::<WirePaint>(&line)
                {
                    answers.heard(&link);
                    #[cfg(target_os = "linux")]
                    {
                        frames.step(crate::dmabuf::Frames::Bgra);
                        pools.lock().unwrap_or_else(PoisonError::into_inner).forget(paint.tab_id);
                    }
                    paint.push(&link.surfaces);
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<EngineMsg>(&line) else { continue };
                answers.heard(&link);
                // Straight to the tab's surface, which wakes the app itself.
                if let EngineMsg::SharedFrame { tab_id, frame_id, width, height, texture, release_fence } = msg {
                    paint_event(PaintEvent::Frame);
                    push_shared(&link.surfaces, tab_id, frame_id, width, height, texture, release_fence);
                    continue;
                }
                #[cfg(target_os = "linux")]
                match msg {
                    EngineMsg::DmabufPool { tab_id, pool, buffers, width, height, stride, offset, modifier } => {
                        let info = crate::dmabuf::PoolInfo { tab_id, pool, buffers, size: [width, height], stride, offset, modifier };
                        pools.lock().unwrap_or_else(PoisonError::into_inner).add(info, reader.take_fds(line_at));
                        continue;
                    }
                    EngineMsg::DmabufFrame { tab_id, frame_id, pool, slot } => {
                        frames.step(crate::dmabuf::Frames::Dmabuf);
                        paint_event(PaintEvent::Frame);
                        let surface = link.surfaces.lock().unwrap_or_else(PoisonError::into_inner).get(&tab_id).cloned();
                        pools.lock().unwrap_or_else(PoisonError::into_inner).show(surface.as_ref(), tab_id, frame_id, pool, slot);
                        continue;
                    }
                    EngineMsg::TabClosed { tab_id } => pools.lock().unwrap_or_else(PoisonError::into_inner).close(tab_id),
                    _ => {}
                }
                if let EngineMsg::CpuFrame { tab_id, frame_id, width, height, bgra_b64 } = msg {
                    #[cfg(target_os = "linux")]
                    pools.lock().unwrap_or_else(PoisonError::into_inner).forget(tab_id);
                    WirePaint { tab_id, frame_id, width, height, bgra_b64: &bgra_b64 }.push(&link.surfaces);
                    continue;
                }
                // Track whether this engine speaks `EngineMsg::PreviewCredit` before any `PreviewFetch`.
                if let EngineMsg::Ready { preview_credit, .. } = &msg {
                    engine_log::log(link.profile.as_deref(), &format!("gen={} ready", link.generation));
                    link.preview_chunk_budget.set_credit_enabled(*preview_credit);
                }
                // Handle per-proxy preview credit/cancel directly on the rx thread so streaming
                // workers wake immediately without waiting for a UI frame.
                if let EngineMsg::PreviewCredit { req_id, bytes, cancel } = msg {
                    link.preview_chunk_budget.on_credit(req_id, bytes as usize, cancel);
                    continue;
                }
                if let EngineMsg::PreviewFetch { req_id, tab_id, .. } = &msg {
                    link.preview_chunk_budget.on_fetch(*req_id, *tab_id);
                }
                if let EngineMsg::TabClosed { tab_id } = &msg {
                    link.preview_chunk_budget.cancel_tab(*tab_id);
                }
                // On reconnect the engine repaints immediately after TabList. Give those paints
                // their destination before the UI thread has a chance to adopt the restored tabs.
                if let EngineMsg::TabList { tabs } = &msg {
                    let mut surfaces = link.surfaces.lock().unwrap_or_else(PoisonError::into_inner);
                    for tab in tabs {
                        surfaces.entry(tab.tab_id).or_default();
                    }
                }
                link.inbox_tx.send(msg).ok();
                link.wake();
            }
            #[cfg(target_os = "linux")]
            {
                let mut slot = link.dmabuf_pools.lock().unwrap_or_else(PoisonError::into_inner);
                if slot.as_ref().is_some_and(|p| Arc::ptr_eq(p, &pools)) {
                    *slot = None;
                }
            }
            link.preview_chunk_budget.reset();
            answers.end();
            lost(&link, None);
        })
        .ok();
    WireResult::Attached
}

#[derive(serde::Deserialize)]
struct WirePaint<'a> {
    tab_id: u64,
    frame_id: u64,
    width: u32,
    height: u32,
    bgra_b64: &'a str,
}

impl WirePaint<'_> {
    fn push(self, surfaces: &Surfaces) {
        let surface = surfaces.lock().unwrap_or_else(PoisonError::into_inner).get(&self.tab_id).cloned();
        if let Some(surface) = surface && let Some(bytes) = decode_frame(self.width, self.height, self.bgra_b64) {
            frame::push_bgra(&surface, self.width, self.height, bytes, self.frame_id);
            paint_event(PaintEvent::Frame);
        }
    }
}

pub(crate) enum PaintEvent { Navigate, Frame }

#[derive(Clone, Copy)]
enum PaintTrace { Start, NavigatedBeforeFirst, AwaitNavigation, AwaitPaint, Complete }

impl PaintTrace {
    fn step(&mut self, event: PaintEvent) -> &'static [&'static str] {
        use PaintTrace::*;
        let (next, marks): (_, &[_]) = match (*self, event) {
            (Start, PaintEvent::Navigate) => (NavigatedBeforeFirst, &[]),
            (Start, PaintEvent::Frame) => (AwaitNavigation, &["browser.page.first_frame"]),
            (NavigatedBeforeFirst, PaintEvent::Frame) => (Complete, &["browser.page.first_frame", "browser.page.painted"]),
            (AwaitNavigation, PaintEvent::Navigate) => (AwaitPaint, &[]),
            (AwaitPaint, PaintEvent::Frame) => (Complete, &["browser.page.painted"]),
            _ => return &[],
        };
        *self = next;
        marks
    }
}

pub(crate) fn paint_event(event: PaintEvent) {
    static TRACE: Mutex<PaintTrace> = Mutex::new(PaintTrace::Start);
    let mut trace = TRACE.lock().unwrap_or_else(PoisonError::into_inner);
    for mark in trace.step(event) {
        rune::marks::mark(mark);
    }
}

/// `on_paint` sends one tightly packed, top-down BGRA plane with standard padded base64.
fn decode_frame(width: u32, height: u32, encoded: &str) -> Option<Vec<u8>> {
    let len = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
    if len == 0 || base64::encoded_len(len, true)? != encoded.len() {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    if bytes.len() != len {
        return None;
    }
    Some(bytes)
}

#[cfg(windows)]
fn push_shared(surfaces: &Surfaces, tab_id: u64, frame_id: u64, width: u32, height: u32, texture: String, release_fence: String) {
    let surface = surfaces.lock().unwrap_or_else(PoisonError::into_inner).get(&tab_id).cloned();
    if let Some(surface) = surface {
        let frame = rune::native_surface::windows::SharedTexture::named(texture, width, height)
            .release_fence(release_fence, frame_id);
        surface.push_shared(frame, frame_id).ok();
    }
}

#[cfg(not(target_os = "linux"))]
enum BoundedLine {
    Line,
    Oversized,
    Eof,
}

#[cfg(not(target_os = "linux"))]
fn read_bounded_wire_line<R: BufRead>(reader: &mut R, raw: &mut Vec<u8>, out: &mut String) -> BoundedLine {
    const MAX_WIRE_LINE_BYTES: usize = 32 * 1024 * 1024;
    raw.clear();
    let mut oversized = false;
    loop {
        let Ok(avail) = reader.fill_buf() else { return BoundedLine::Eof };
        if avail.is_empty() {
            if raw.is_empty() && !oversized {
                return BoundedLine::Eof;
            }
            break;
        }
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            if !oversized && raw.len() + pos <= MAX_WIRE_LINE_BYTES {
                raw.extend_from_slice(&avail[..pos]);
            } else {
                oversized = true;
            }
            reader.consume(pos + 1);
            break;
        }
        let n = avail.len();
        if !oversized && raw.len() + n <= MAX_WIRE_LINE_BYTES {
            raw.extend_from_slice(avail);
        } else {
            oversized = true;
            raw.clear();
        }
        reader.consume(n);
    }
    if oversized {
        return BoundedLine::Oversized;
    }
    match std::str::from_utf8(raw) {
        Ok(s) => {
            out.push_str(s);
            BoundedLine::Line
        }
        Err(_) => BoundedLine::Oversized,
    }
}

/// Only a Windows engine sends shared textures.
#[cfg(not(windows))]
fn push_shared(_: &Surfaces, _: u64, _: u64, _: u32, _: u32, _: String, _: String) {}
