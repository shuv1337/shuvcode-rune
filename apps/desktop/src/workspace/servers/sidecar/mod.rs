//! Process-wide local OpenCode background service manager
//! (packages/desktop/src/main/service/background-service.ts,
//! background-service-machine.ts, sidecar-probe.ts).
//!
//! Driven by the explicit opencode_servers::sidecar state machine:
//! - On launch, probes service.json for an already-running compatible service; if found, adopts
//!   it immediately and verifies/upgrades it in the background.
//! - If none is running (or the registered service is dead/incompatible), spawns
//!   opencode serve --service detached via service::ensure and waits for it to register and
//!   answer /api/info.
//! - Every window shares the single in-flight connect/reconnect attempt and moves automatically
//!   whenever Effect::Publish reports a new origin or password.

#![deny(unsafe_code)]

mod cli;
mod process;
mod pty_handoff;
mod service;
mod shell_env;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once, OnceLock, mpsc};

use opencode_kit::client::Server;
use opencode_kit::servers::HttpBase;
use opencode_servers::sidecar::{self as machine, ConnectMode, Effect, Event, ReplyId};
use rune::machine::Attempt;

struct Inner {
    state: machine::State,
    next_reply: ReplyId,
    waiters: HashMap<ReplyId, mpsc::Sender<Result<HttpBase, String>>>,
    published: Option<HttpBase>,
    published_version: u64,
    wakers: Vec<rune::Waker>,
}

struct Manager {
    inner: Mutex<Inner>,
    disposed: AtomicBool,
}

static MANAGER: OnceLock<Manager> = OnceLock::new();
static STARTED: Once = Once::new();

/// Whether this process manages its own local OpenCode background service (windowed runs with no
/// explicit --server, OPENCODE_SERVER_URL, --fixture, or --demo).
pub fn managed() -> bool {
    // shuv1337 fork: never install or start a local OpenCode server. We connect only to servers
    // already running (local shuvcode, the shuvbot host), as before opencode-rune#1303. Every
    // sidecar entry point is gated on this, so returning false keeps the upstream code dormant.
    false
}

/// Starts the background service machine at process boot (before the first window opens),
/// matching startSidecarProbe + BackgroundServiceMachine.make in official Desktop.
pub fn start() {
    if !managed() {
        return;
    }
    let _ = manager();
}

/// Waits for (or starts) the local background service (BackgroundService.connection).
pub fn connection() -> Result<Server, String> {
    request(false)
}

/// Reconnects to (or restarts) the local background service (BackgroundService.reconnect).
pub fn reconnect() -> Result<Server, String> {
    request(true)
}

/// Latest published local background service, if any (non-blocking).
pub fn current() -> Option<Server> {
    let m = MANAGER.get()?;
    let inner = m.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    inner.published.as_ref().map(server_from_http)
}

/// Monotonic version bumped whenever Effect::Publish updates the active sidecar endpoint.
pub fn published_version() -> u64 {
    let Some(m) = MANAGER.get() else { return 0 };
    m.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).published_version
}

/// Registers a window waker so background Publish events (e.g. Verify replacing an older
/// service) wake the window promptly.
pub fn register_wake(wake: rune::Waker) {
    let Some(m) = MANAGER.get() else { return };
    let mut inner = m.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if inner.wakers.len() >= 16 {
        inner.wakers.remove(0);
    }
    inner.wakers.push(wake);
}

/// Disposes waiters on app quit without stopping the detached background service daemon
/// (matching official background-service-machine.ts:346).
pub fn dispose() {
    let Some(m) = MANAGER.get() else { return };
    m.disposed.store(true, Ordering::Relaxed);
    dispatch(m, Event::Disposed);
}

fn server_from_http(http: &HttpBase) -> Server {
    let s = Server::anonymous(&http.url);
    match &http.password {
        Some(pw) if !pw.is_empty() => s.with_password(pw),
        _ => s,
    }
}

fn manager() -> &'static Manager {
    let initial_connect = Mutex::new(None);
    let mgr = MANAGER.get_or_init(|| {
        let (state, initial_fx) = machine::State::new();
        for effect in initial_fx {
            if let Effect::Connect { attempt, mode } = effect {
                *initial_connect.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((attempt, mode));
            }
        }
        Manager {
            inner: Mutex::new(Inner {
                state,
                next_reply: 0,
                waiters: HashMap::new(),
                published: None,
                published_version: 0,
                wakers: Vec::new(),
            }),
            disposed: AtomicBool::new(false),
        }
    });
    // Start the initial connect worker only after MANAGER is initialized so the thread never
    // observes MANAGER.get() == None.
    STARTED.call_once(|| {
        let (attempt, mode) = initial_connect
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap_or((Attempt(1), ConnectMode::Initial));
        spawn_connect(attempt, mode);
    });
    mgr
}

fn request(is_reconnect: bool) -> Result<Server, String> {
    let m = manager();
    let (tx, rx) = mpsc::channel();
    let reply_id = {
        let mut inner = m.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.next_reply = inner.next_reply.wrapping_add(1);
        let id = inner.next_reply;
        inner.waiters.insert(id, tx);
        id
    };
    let event = if is_reconnect { Event::ReconnectRequested(reply_id) } else { Event::ConnectionRequested(reply_id) };
    dispatch(m, event);
    rx.recv()
        .unwrap_or_else(|_| Err(machine::DISPOSED_ERROR.into()))
        .map(|http| server_from_http(&http))
}

fn dispatch(m: &Manager, event: Event) {
    let mut replies_ok: Vec<(mpsc::Sender<Result<HttpBase, String>>, HttpBase)> = Vec::new();
    let mut replies_err: Vec<(mpsc::Sender<Result<HttpBase, String>>, String)> = Vec::new();
    let mut connects: Vec<(Attempt, ConnectMode)> = Vec::new();
    let mut verifies: Vec<Attempt> = Vec::new();
    let mut wake_list: Vec<rune::Waker> = Vec::new();

    {
        let mut inner = m.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let fx = machine::step(&mut inner.state, event);
        for effect in fx {
            match effect {
                Effect::Connect { attempt, mode } => connects.push((attempt, mode)),
                Effect::Verify { attempt } => verifies.push(attempt),
                Effect::Publish(data) => {
                    inner.published = Some(data);
                    inner.published_version = inner.published_version.wrapping_add(1);
                    wake_list = inner.wakers.clone();
                }
                Effect::Resolve { replies, data } => {
                    for id in replies {
                        if let Some(tx) = inner.waiters.remove(&id) {
                            replies_ok.push((tx, data.clone()));
                        }
                    }
                }
                Effect::Reject { replies, error } => {
                    for id in replies {
                        if let Some(tx) = inner.waiters.remove(&id) {
                            replies_err.push((tx, error.clone()));
                        }
                    }
                }
            }
        }
    }

    for (tx, data) in replies_ok {
        tx.send(Ok(data)).ok();
    }
    for (tx, err) in replies_err {
        tx.send(Err(err)).ok();
    }
    for w in wake_list {
        w();
    }
    for (attempt, mode) in connects {
        spawn_connect(attempt, mode);
    }
    for attempt in verifies {
        spawn_verify(attempt);
    }
}

fn spawn_connect(attempt: Attempt, mode: ConnectMode) {
    let _ = std::thread::Builder::new().name("opencode-sidecar".into()).spawn(move || {
        let Some(m) = MANAGER.get() else { return };
        if mode == ConnectMode::Initial
            && let Some(data) = probe_initial()
        {
            dispatch(m, Event::ProbeSucceeded { attempt, data });
            return;
        }
        match ensure_service(mode, &m.disposed) {
            Ok(data) => dispatch(m, Event::ConnectSucceeded { attempt, data }),
            Err(error) => {
                eprintln!("opencode-rune: local background service failed: {error}");
                dispatch(m, Event::ConnectFailed { attempt, error });
            }
        }
    });
}

fn spawn_verify(attempt: Attempt) {
    let _ = std::thread::Builder::new().name("opencode-sidecar-verify".into()).spawn(move || {
        let Some(m) = MANAGER.get() else { return };
        match ensure_service(ConnectMode::Initial, &m.disposed) {
            Ok(data) => dispatch(m, Event::VerifySucceeded { attempt, data }),
            Err(error) => dispatch(m, Event::VerifyFailed { attempt, error }),
        }
    });
}

/// Fast non-spawning initial probe (packages/desktop/src/main/service/sidecar-probe.ts,
/// background-service.ts:44-50).
fn probe_initial() -> Option<HttpBase> {
    let (reuse, isolated) = server_flags();
    let file = registration_file(reuse, isolated)?;
    let required = if reuse || isolated { None } else { cli::bundled_probe_version() };
    let endpoint = service::discover(&file, required.as_deref())?;
    let password = endpoint.password.filter(|p| !p.is_empty())?;
    let origin = service::normalize_origin(&endpoint.url)?;
    Some(HttpBase { url: origin, password: Some(password) })
}

fn server_flags() -> (bool, bool) {
    let packaged = !cfg!(debug_assertions);
    let flag = |k: &str| std::env::var_os(k).is_some_and(|v| v == "1");
    let reuse = !packaged && flag("OPENCODE_DESKTOP_REUSE_SERVER");
    let isolated = !packaged && !reuse && flag("OPENCODE_DESKTOP_ISOLATED_SERVER");
    (reuse, isolated)
}

fn registration_file(reuse: bool, isolated: bool) -> Option<PathBuf> {
    if reuse {
        if let Some(custom) = std::env::var_os("OPENCODE_DESKTOP_REUSE_SERVICE_FILE").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(custom));
        }
        return service::fallback_file();
    }
    let state = if isolated { crate::dirs::data()? } else { service::state_dir()? };
    let local = std::env::var_os("OPENCODE_DESKTOP_SERVER_CHANNEL").is_some_and(|v| v == "local");
    let name = if local { "service-local.json" } else { "service.json" };
    Some(state.join("opencode").join(name))
}

/// BackgroundService.ensure (packages/desktop/src/main/service/background-service.ts:52-156).
fn ensure_service(mode: ConnectMode, disposed: &AtomicBool) -> Result<HttpBase, String> {
    let (reuse, isolated) = server_flags();
    let env = shell_env::service_env(isolated);
    let resolved = cli::resolve(&env, isolated)?;
    let file = registration_file(reuse, isolated).ok_or_else(|| "Could not resolve service registration path".to_owned())?;

    let version = if reuse || !resolved.bundled {
        None
    } else {
        resolved.version.as_deref().and_then(|v| cli::service_version(isolated, mode, v))
    };

    let mut command = resolved.command.clone();
    command.push("serve".into());
    command.push("--service".into());
    if isolated {
        // 0x0c0c = 3084 (background-service.ts:67): fixed port prevents two isolated servers
        // from booting against the same database.
        command.push("--hostname".into());
        command.push("0.0.0.0".into());
        command.push("--port".into());
        command.push(0x0c0c_u16.to_string());
    }

    let run = |req_version: Option<&str>| {
        service::ensure(
            &service::EnsureOptions {
                file: &file,
                version: req_version,
                command: &command,
                env: &env,
                timing: service::DEFAULT_TIMING,
            },
            disposed,
        )
    };

    let endpoint = match run(version.as_deref()) {
        Ok(ep) => ep,
        Err(err)
            if resolved.bundled
                && cli::should_retry_ensure(version.as_deref(), &err)
                && resolved.version.is_some() =>
        {
            run(resolved.version.as_deref())?
        }
        Err(err) => return Err(err),
    };

    let password = endpoint
        .password
        .filter(|p| !p.is_empty())
        .ok_or_else(|| "V2 CLI background service did not provide authentication".to_owned())?;
    let origin = service::normalize_origin(&endpoint.url)
        .ok_or_else(|| format!("Invalid background service URL: {}", endpoint.url))?;

    let running_version = service::read_info(&file).and_then(|i| i.version);
    if mode == ConnectMode::Initial
        && let (Some(staged), Some(bundled_ver)) = (resolved.staged_binary.clone(), resolved.version.clone())
        && let Some(running_ver) = running_version
    {
        let keep = cli::kept_stages(&bundled_ver, Some(&running_ver));
        let _ = std::thread::Builder::new()
            .name("opencode-cli-clean".into())
            .spawn(move || cli::clean_stages(&staged, &keep));
    }

    Ok(HttpBase { url: origin, password: Some(password) })
}
