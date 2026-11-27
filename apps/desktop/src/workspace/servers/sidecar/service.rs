//! Local OpenCode background service discovery, ensuring, contender management, and termination
//! (packages/client/src/promise/service.ts, service-probe.ts, service-contender.ts,
//! service-timing.ts, service-version.ts).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use rune::time::Instant;
use serde::Deserialize;
use serde_json::Value;

use super::{process, pty_handoff};

/// Info (packages/client/src/service.ts:47-58).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(super) struct Info {
    #[serde(default)]
    pub(super) id: Option<String>,
    #[serde(default)]
    pub(super) version: Option<String>,
    pub(super) url: String,
    pub(super) pid: u32,
    #[serde(default)]
    pub(super) password: Option<String>,
}

/// Endpoint (packages/client/src/service.ts:2-13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Endpoint {
    pub(super) url: String,
    pub(super) password: Option<String>,
}

/// LocalService.state (packages/client/src/service-probe.ts:14).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ServiceState {
    Ready,
    Waiting,
    Failed,
}

/// LocalService (packages/client/src/service-probe.ts:10-16).
#[derive(Clone, Debug)]
pub(super) struct LocalService {
    pub(super) info: Info,
    pub(super) endpoint: Endpoint,
    pub(super) version: Option<String>,
    pub(super) state: ServiceState,
    pub(super) compatible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PtyMode {
    Clear,
    Handoff,
}

/// decide outcome (packages/client/src/service-probe.ts:22-32).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Reuse,
    Wait,
    Replace { pty: PtyMode },
    Fail(String),
}

/// EnsureTiming (packages/client/src/service-timing.ts:1-25).
#[derive(Clone, Copy, Debug)]
pub(super) struct EnsureTiming {
    pub(super) poll_interval: Duration,
    pub(super) request_timeout: Duration,
    pub(super) spawn_delay: Duration,
    pub(super) max_spawn_delay: Duration,
    pub(super) promise_timeout: Duration,
    pub(super) stop_poll_interval: Duration,
    pub(super) stop_poll_attempts: u32,
}

pub(super) const DEFAULT_TIMING: EnsureTiming = EnsureTiming {
    poll_interval: Duration::from_millis(25),
    request_timeout: Duration::from_secs(2),
    spawn_delay: Duration::from_secs(5),
    max_spawn_delay: Duration::from_secs(30),
    promise_timeout: Duration::from_secs(120),
    stop_poll_interval: Duration::from_millis(50),
    stop_poll_attempts: 100,
};

pub(super) struct EnsureOptions<'a> {
    pub(super) file: &'a Path,
    pub(super) version: Option<&'a str>,
    pub(super) command: &'a [String],
    pub(super) env: &'a BTreeMap<String, String>,
    pub(super) timing: EnsureTiming,
}

/// fallback() (packages/client/src/service-probe.ts:39-41):
/// <XDG_STATE_HOME || <home>/.local/state>/opencode/service.json.
pub(super) fn fallback_file() -> Option<PathBuf> {
    state_dir().map(|d| d.join("opencode").join("service.json"))
}

pub(super) fn state_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg));
    }
    let home = rune::dirs::home_from(|k| std::env::var_os(k))?;
    Some(home.join(".local").join("state"))
}

/// Normalizes a local service URL to <scheme>://<host>[:<port>], mapping 0.0.0.0 to
/// 127.0.0.1 (packages/desktop/src/main/service/background-service.ts:133-135).
pub(super) fn normalize_origin(raw: &str) -> Option<String> {
    let mut parsed = url::Url::parse(raw.trim()).ok()?;
    if parsed.host_str() == Some("0.0.0.0") {
        let _ = parsed.set_host(Some("127.0.0.1"));
    }
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    match parsed.port() {
        Some(port) => Some(format!("{scheme}://{host}:{port}")),
        None => Some(format!("{scheme}://{host}")),
    }
}

/// headers() (packages/client/src/service-probe.ts:90-95).
pub(super) fn auth_header(password: Option<&str>) -> Option<String> {
    let pw = password.filter(|p| !p.is_empty())?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("opencode:{pw}"));
    Some(format!("Basic {encoded}"))
}

/// same() (packages/client/src/service-probe.ts:44-46).
fn same_info(left: &Info, right: &Info) -> bool {
    left.id == right.id && left.version == right.version && left.url == right.url && left.pid == right.pid
}

/// matchesVersion() (packages/client/src/service-version.ts:3-8).
pub(super) fn matches_version(version: Option<&str>, required: Option<&str>) -> bool {
    let Some(req) = required else { return true };
    version == Some(req)
}

/// decide() (packages/client/src/service-probe.ts:22-32).
pub(super) fn decide(service: &LocalService, required_version: Option<&str>) -> Decision {
    if !matches_version(service.version.as_deref(), required_version) {
        let pty = if service.state == ServiceState::Ready { PtyMode::Handoff } else { PtyMode::Clear };
        return Decision::Replace { pty };
    }
    if !service.compatible {
        return Decision::Fail(
            "Background service uses an incompatible health protocol. Update this client or explicitly restart the service."
                .into(),
        );
    }
    match service.state {
        ServiceState::Ready => Decision::Reuse,
        ServiceState::Failed => Decision::Fail("Background service failed to start".into()),
        ServiceState::Waiting => Decision::Wait,
    }
}

/// Reads and parses service.json.
pub(super) fn read_info(file: &Path) -> Option<Info> {
    let text = std::fs::read_to_string(file).ok()?;
    serde_json::from_str::<Info>(&text).ok()
}

/// probeResult() (packages/client/src/service-probe.ts:48-87).
pub(super) fn probe_result(info: &Info, timeout: Duration) -> (Option<LocalService>, bool) {
    let Some(origin) = normalize_origin(&info.url) else {
        return (None, false);
    };
    let endpoint = Endpoint { url: origin.clone(), password: info.password.clone() };
    let agent: ureq::Agent = net_trust::agent_config_for(&origin)
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None)
        .build()
        .into();
    let mut req = agent.get(format!("{origin}/api/info"));
    if let Some(auth) = auth_header(info.password.as_deref()) {
        req = req.header("Authorization", &auth);
    }
    let mut resp = match req.call() {
        Ok(r) => r,
        Err(e) => return (None, matches!(e, ureq::Error::Timeout(_))),
    };
    let status = resp.status().as_u16();
    if status == 404 {
        return (
            Some(LocalService {
                info: info.clone(),
                endpoint,
                version: info.version.clone(),
                state: ServiceState::Ready,
                compatible: false,
            }),
            false,
        );
    }
    let Ok(bytes) = resp.body_mut().with_config().limit(256 << 10).read_to_vec() else {
        return (None, false);
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return (None, false);
    };
    let Some(version) = body.get("version").and_then(Value::as_str).map(str::to_owned) else {
        return (None, false);
    };
    let Some(pid) = body.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()) else {
        return (None, false);
    };
    if pid != info.pid {
        return (None, false);
    }
    if let Some(expected) = &info.version
        && *expected != version
    {
        return (None, false);
    }
    let state = match status {
        200..=299 => ServiceState::Ready,
        500 => ServiceState::Failed,
        _ => ServiceState::Waiting,
    };
    (Some(LocalService { info: info.clone(), endpoint, version: Some(version), state, compatible: true }), false)
}

struct Registered {
    info: Option<Info>,
    service: Option<LocalService>,
    timed_out: bool,
}

fn registered(file: &Path, timeout: Duration) -> Registered {
    let Some(info) = read_info(file) else {
        return Registered { info: None, service: None, timed_out: false };
    };
    let (service, timed_out) = probe_result(&info, timeout);
    Registered { info: Some(info), service, timed_out }
}

/// Service.discover() (packages/client/src/promise/service.ts:16-22).
pub(super) fn discover(file: &Path, required_version: Option<&str>) -> Option<Endpoint> {
    let found = registered(file, DEFAULT_TIMING.request_timeout).service?;
    if found.state != ServiceState::Ready || !found.compatible {
        return None;
    }
    if !matches_version(found.version.as_deref(), required_version) {
        return None;
    }
    Some(found.endpoint)
}

const STDERR_LIMIT: usize = 8 * 1024;

/// A spawned opencode serve --service contender (packages/client/src/service-contender.ts:4-55).
struct Contender {
    child: Child,
    pid: u32,
    stderr: Arc<Mutex<Vec<u8>>>,
    released: Arc<AtomicBool>,
    exited: Option<ExitStatus>,
}

impl Contender {
    fn spawn(command: &[String], env: &BTreeMap<String, String>) -> Result<Self, String> {
        let Some((program, args)) = command.split_first() else {
            return Err("Missing service command".into());
        };
        let mut child = process::spawn_detached(program, args, env)
            .map_err(|e| format!("Failed to start server: {e}"))?;
        let pid = child.id();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let released = Arc::new(AtomicBool::new(false));
        if let Some(mut pipe) = child.stderr.take() {
            let buf = stderr.clone();
            let flag = released.clone();
            // Drain stderr until EOF even after release so the detached server never hits SIGPIPE.
            std::thread::Builder::new()
                .name("opencode-svc-stderr".into())
                .spawn(move || {
                    let mut chunk = [0u8; 2048];
                    loop {
                        match pipe.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if flag.load(Ordering::Relaxed) {
                                    continue;
                                }
                                let mut dst = buf.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                let slice = &chunk[..n];
                                if slice.len() >= STDERR_LIMIT {
                                    dst.clear();
                                    dst.extend_from_slice(&slice[slice.len() - STDERR_LIMIT..]);
                                } else {
                                    let overflow = (dst.len() + slice.len()).saturating_sub(STDERR_LIMIT);
                                    if overflow > 0 {
                                        dst.drain(..overflow);
                                    }
                                    dst.extend_from_slice(slice);
                                }
                            }
                        }
                    }
                })
                .ok();
        }
        Ok(Self { child, pid, stderr, released, exited: None })
    }

    fn poll_exited(&mut self) -> Option<ExitStatus> {
        if let Some(status) = self.exited {
            return Some(status);
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exited = Some(status);
                Some(status)
            }
            _ => None,
        }
    }

    fn stderr_text(&self) -> String {
        let bytes = self.stderr.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).trim().to_owned()
    }

    fn release(&self) {
        self.released.store(true, Ordering::Relaxed);
        self.stderr.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
    }
}

/// Explicit spawn pacing clock for ContenderPool (packages/client/src/service-contender.ts:62-117).
enum SpawnClock {
    /// No contender spawned yet: a live existing registration gets one spawn_delay grace first;
    /// otherwise spawn immediately.
    Initial,
    /// Waiting until since + spawn_delay before recruiting another contender.
    Cooldown(Instant),
    /// Next recruit runs immediately (after evicting an unresponsive owner).
    ReadyNow,
}

/// contenderPool() (packages/client/src/service-contender.ts:61-118).
struct ContenderPool {
    contenders: Vec<Contender>,
    failure: Option<String>,
    timing: EnsureTiming,
    spawn_delay: Duration,
    clock: SpawnClock,
}

impl ContenderPool {
    fn new(timing: EnsureTiming) -> Self {
        Self {
            contenders: Vec::new(),
            failure: None,
            timing,
            spawn_delay: timing.spawn_delay,
            clock: SpawnClock::Initial,
        }
    }

    fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    fn service_answered(&mut self) {
        self.spawn_delay = self.timing.spawn_delay;
    }

    fn evict(&mut self, pid: u32) {
        let mut kept = Vec::with_capacity(self.contenders.len());
        for mut item in self.contenders.drain(..) {
            if item.pid == pid || item.poll_exited().is_some() {
                item.release();
            } else {
                kept.push(item);
            }
        }
        self.contenders = kept;
        self.failure = None;
        self.clock = SpawnClock::Initial;
    }

    fn recruit_now(&mut self) {
        self.clock = SpawnClock::ReadyNow;
    }

    fn reap(&mut self) -> Option<String> {
        let mut kept = Vec::with_capacity(self.contenders.len());
        let mut any_clean_exit = false;
        for mut item in self.contenders.drain(..) {
            if let Some(status) = item.poll_exited() {
                if status.success() {
                    any_clean_exit = true;
                } else if self.failure.is_none() {
                    self.failure = Some(contender_failure(status, &item.stderr_text()));
                }
                item.release();
            } else {
                kept.push(item);
            }
        }
        if any_clean_exit {
            self.spawn_delay = (self.spawn_delay * 2).min(self.timing.max_spawn_delay);
        }
        self.contenders = kept;
        if self.contenders.is_empty() { self.failure.clone() } else { None }
    }

    fn should_recruit(&mut self, live_registration: bool) -> bool {
        if self.failure.is_some() || self.contenders.len() >= 2 {
            return false;
        }
        match self.clock {
            SpawnClock::Initial => {
                if live_registration {
                    self.clock = SpawnClock::Cooldown(Instant::now());
                    false
                } else {
                    true
                }
            }
            SpawnClock::ReadyNow => true,
            SpawnClock::Cooldown(since) => since.elapsed() >= self.spawn_delay,
        }
    }

    fn add(&mut self, contender: Contender) {
        self.contenders.push(contender);
        self.clock = SpawnClock::Cooldown(Instant::now());
    }
}

impl Drop for ContenderPool {
    fn drop(&mut self) {
        for item in &self.contenders {
            item.release();
        }
    }
}

fn contender_failure(status: ExitStatus, stderr: &str) -> String {
    let base = match status.code() {
        Some(code) => format!("Server process exited with code {code}"),
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                match status.signal() {
                    Some(sig) => format!("Server process terminated by signal {sig}"),
                    None => "Server process terminated unexpectedly".to_owned(),
                }
            }
            #[cfg(not(unix))]
            {
                "Server process terminated unexpectedly".to_owned()
            }
        }
    };
    if stderr.is_empty() { base } else { format!("{base}\n{stderr}") }
}

/// Service.ensure() (packages/client/src/promise/service.ts:24-95).
pub(super) fn ensure(opts: &EnsureOptions<'_>, disposed: &AtomicBool) -> Result<Endpoint, String> {
    let deadline = Instant::now() + opts.timing.promise_timeout;
    let mut pool = ContenderPool::new(opts.timing);
    let mut timeouts: Option<(Info, u32)> = None;
    let mut announced = false;

    loop {
        if disposed.load(Ordering::Relaxed) {
            return Err(opencode_servers::sidecar::DISPOSED_ERROR.into());
        }
        if Instant::now() >= deadline {
            return Err(pool
                .failure()
                .map(str::to_owned)
                .unwrap_or_else(|| "Timed out waiting for the background service to start".into()));
        }

        let reg = registered(opts.file, opts.timing.request_timeout);
        if reg.timed_out
            && let Some(info) = &reg.info
        {
            let count = match &timeouts {
                Some((prev, n)) if same_info(prev, info) => n + 1,
                _ => 1,
            };
            timeouts = Some((info.clone(), count));
            if count >= 3 {
                if !announced {
                    announced = true;
                    eprintln!("opencode-rune: background service is unresponsive; replacing it");
                }
                pty_handoff::clear(opts.file);
                terminate(opts.file, info, opts.timing)?;
                pool.evict(info.pid);
                pool.recruit_now();
                timeouts = None;
            }
        } else {
            timeouts = None;
        }

        if let Some(svc) = reg.service {
            pool.service_answered();
            match decide(&svc, opts.version) {
                Decision::Fail(err) => return Err(err),
                Decision::Reuse => {
                    pty_handoff::complete(opts.file, &svc.info);
                    return Ok(svc.endpoint);
                }
                Decision::Replace { pty } => {
                    if !announced {
                        announced = true;
                        eprintln!(
                            "opencode-rune: replacing background service (version {:?} -> {:?})",
                            svc.version, opts.version
                        );
                    }
                    let _ = stop(opts.file, pty, opts.timing);
                    pool.evict(svc.info.pid);
                }
                Decision::Wait => {}
            }
        } else {
            if let Some(failed) = pool.reap() {
                return Err(failed);
            }
            // Match service-contender.ts:100-108: give a live booting registration one spawn
            // delay before competing, while a stale registration from a dead PID recruits at once.
            let live_reg = reg.info.as_ref().is_some_and(|i| process::is_alive(i.pid));
            if pool.should_recruit(live_reg) {
                if !announced {
                    announced = true;
                    eprintln!("opencode-rune: starting local OpenCode background service");
                }
                let child_env = pty_handoff::environment(opts.file, opts.env);
                pool.add(Contender::spawn(opts.command, &child_env)?);
            }
        }

        std::thread::sleep(opts.timing.poll_interval);
    }
}

/// Service.stop() (packages/client/src/promise/service.ts:98-107).
pub(super) fn stop(file: &Path, pty: PtyMode, timing: EnsureTiming) -> Result<(), String> {
    let info = read_info(file);
    match (pty, &info) {
        (PtyMode::Handoff, Some(info)) => pty_handoff::prepare(file, info, DEFAULT_TIMING.request_timeout),
        _ => pty_handoff::clear(file),
    }
    if let Some(info) = info {
        terminate(file, &info, timing)?;
    }
    Ok(())
}

fn wait_until_stopped(pid: u32, timing: EnsureTiming) -> bool {
    for attempt in 0..=timing.stop_poll_attempts {
        if !process::is_alive(pid) {
            return true;
        }
        if attempt < timing.stop_poll_attempts {
            std::thread::sleep(timing.stop_poll_interval);
        }
    }
    false
}

/// terminate() (packages/client/src/promise/service.ts:145-158).
fn terminate(file: &Path, info: &Info, timing: EnsureTiming) -> Result<(), String> {
    let Some(current) = read_info(file) else { return Ok(()) };
    if !same_info(&current, info) {
        return Ok(());
    }
    process::terminate_graceful(info.pid);
    if !wait_until_stopped(info.pid, timing) {
        process::terminate_force(info.pid);
        if !wait_until_stopped(info.pid, timing) {
            return Err(format!("Server process {} is still running", info.pid));
        }
    }
    if let Some(latest) = read_info(file)
        && same_info(&latest, info)
    {
        let _ = std::fs::remove_file(file);
    }
    Ok(())
}
