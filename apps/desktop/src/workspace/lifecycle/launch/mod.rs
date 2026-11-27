//! Launch order. A window's first frame draws while the window is created, so a windowed live
//! run reads only local files before it (saved tabs, zoom, route and drafts). `on_ready` then
//! starts the first server reads; when they land, the event stream, the open session's page and
//! the prefetch start (`boot::connect`). Headless runs have no `on_ready`: they read the server
//! before the first frame, as a snapshot needs, and connect from `setup`.

mod boot;
#[cfg(target_os = "windows")]
pub mod compat_layer;
#[cfg(target_os = "windows")]
pub mod image_pages;
#[cfg(target_os = "linux")]
#[path = "image_pages_linux.rs"]
pub mod image_pages;
#[cfg(target_os = "windows")]
pub mod io_trace;
pub mod last_exit;
mod network;
mod onboarding;
pub mod saved;
mod shifts;
pub mod trace;

#[cfg(target_arch = "wasm32")]
pub use boot::start_first_reads;
pub use saved::{catalog as saved_catalog, gutters as saved_gutters, list as saved_list};
#[cfg(windows)]
#[cfg(windows)]
pub use boot::leave_install_folder;
#[cfg(windows)]
pub use boot::warm_gpu_if_asked;
pub use boot::{
    Accepted, FIRST_PATIENCE, First, GRACE, Ready, defer, deferred, initial_server, window_title,
};
#[cfg(not(target_arch = "wasm32"))]
pub use boot::{
    apply_app_icon, attach_console, head_start, instance_id, print_version_if_asked, product, unwind_check_if_asked, write_profile,
};

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use opencode_kit::client::Server;
use rune::Tx;
use rune::machine::Effects;
use rune::time::Instant;

use crate::reconnect::ServerId;
use crate::{App, Route, Source};

/// A server that didn't answer, trying it again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Again {
    pub retries: u32,
    /// The app tried on its own (its timer, the network coming back): the card's button doesn't
    /// say Checking, and another miss changes nothing on screen.
    pub quiet: bool,
}

/// One server's launch: from the first reads to connected, or no answer and tries again. As
/// OpenCode Desktop's server health settles `checking → settled` per server
/// (`packages/app/src/runtime/server/health-machine.ts`), and its startup waits for the server
/// before it mounts (`packages/desktop/src/renderer/startup/machine.ts`: `booting → hydrating`).
#[derive(Debug, Default)]
pub enum Phase {
    /// A windowed run waits for `on_ready` before its first reads. `beta_tabs`: no tabs were
    /// saved, so Desktop Beta's `drafts.sqlite` tabs are read too; without them a first launch
    /// has none and shows Home, as Desktop's does. `fallback_fixture`: fall back to the built-in
    /// fixture when the initial probe finds no server (sidecar only).
    Waiting { beta_tabs: bool, fallback_fixture: bool },
    /// The probe and first reads are on their way; `again` while the server didn't answer before.
    /// `accepted`: when the server took the probe's connection, so the reads that follow are
    /// waiting on it (`FIRST_PATIENCE`). `defer_plugins`: on `wasm32`, `ready()` held back the
    /// off-first-screen built-ins so starting them on the single browser thread does not delay
    /// these reads' answers.
    Probing { rx: Receiver<Ready>, again: Option<Again>, accepted: Accepted, defer_plugins: bool },
    /// No server answered: the next try is at `retry_at` (`App::clock`), or, when nothing
    /// accepted a connection there (`retry_at` infinite), once its health says it answers.
    Offline { retries: u32, retry_at: f64 },
    /// The first reads landed (a headless run reads them before its first frame).
    #[default]
    Connected,
}

pub enum LaunchEvent {
    /// `on_ready`, or `setup` for a run that read the server already.
    Start,
    /// On `wasm32`, `ready()` ran while probing and deferred non-first-screen built-ins until the
    /// probe settles.
    DeferPlugins,
    /// Retry: the card's button (`quiet` false), or the app on its own.
    Retry { quiet: bool },
    /// The network came back: start the backoff over.
    NetworkUp,
    /// The window polls at `now`.
    Tick { now: f64 },
    /// The server's health check answers again (the reach watcher saw it accept, or the 10 s
    /// poll reached it).
    Answering,
    /// The probe answered: the server answered (`Some(true)`), didn't (`Some(false)`), or the
    /// window falls back to the fixture (`None`); `refused` when nothing accepted a connection.
    Landed { reached: Option<bool>, refused: bool, now: f64 },
}

#[derive(Debug, PartialEq)]
pub enum LaunchEffect {
    /// Start the probe and first reads, keeping on for `grace`; the caller enters `Probing`.
    Probe { beta_tabs: bool, fallback_fixture: bool, again: Option<Again>, grace: Duration },
    /// The first reads are in: connect the stream and the open session.
    Connect,
    /// No server answered, first said: the card and the session's unavailable panel show.
    Unreachable,
    /// The fixture stands in for a server that isn't there.
    Fixture,
    /// Start the off-first-screen built-ins that `ready()` deferred while probing (`wasm32`).
    StartPlugins,
    WakeAt(f64),
    /// Something on screen changed.
    Repaint,
}

impl Phase {
    pub fn step(&mut self, event: LaunchEvent, grace: Duration) -> Effects<LaunchEffect> {
        use LaunchEffect::*;
        match (std::mem::take(self), event) {
            (Phase::Waiting { beta_tabs, fallback_fixture }, LaunchEvent::Start) => {
                Effects::one(Probe { beta_tabs, fallback_fixture, again: None, grace })
            }
            (Phase::Connected, LaunchEvent::Start) => Effects::one(Connect),
            (Phase::Probing { rx, again, accepted, .. }, LaunchEvent::DeferPlugins) => {
                *self = Phase::Probing { rx, again, accepted, defer_plugins: true };
                Effects::none()
            }
            (Phase::Offline { retries, .. }, LaunchEvent::Retry { quiet }) => {
                Effects::two(Repaint, Probe { beta_tabs: false, fallback_fixture: false, again: Some(Again { retries, quiet }), grace: if quiet { Duration::ZERO } else { grace } })
            }
            (Phase::Offline { .. }, LaunchEvent::NetworkUp) => Effects::one(Probe { beta_tabs: false, fallback_fixture: false, again: Some(Again { retries: 0, quiet: true }), grace: Duration::ZERO }),
            (Phase::Offline { retries, retry_at }, LaunchEvent::Answering) if retry_at.is_infinite() => {
                Effects::one(Probe { beta_tabs: false, fallback_fixture: false, again: Some(Again { retries, quiet: true }), grace: Duration::ZERO })
            }
            (Phase::Offline { retries, retry_at }, LaunchEvent::Tick { now }) if now + 1e-3 >= retry_at => {
                Effects::one(Probe { beta_tabs: false, fallback_fixture: false, again: Some(Again { retries, quiet: true }), grace: Duration::ZERO })
            }
            (Phase::Offline { retries, retry_at }, LaunchEvent::Tick { .. }) => {
                *self = Phase::Offline { retries, retry_at };
                Effects::none()
            }
            (Phase::Probing { again, defer_plugins, .. }, LaunchEvent::Landed { reached: Some(true), .. }) => {
                *self = Phase::Connected;
                let _ = again;
                let mut effects = Effects::two(Repaint, Connect);
                effects.extend(defer_plugins.then_some(StartPlugins));
                effects
            }
            (Phase::Probing { defer_plugins, .. }, LaunchEvent::Landed { reached: None, .. }) => {
                *self = Phase::Connected;
                let mut effects = Effects::two(Repaint, Fixture);
                effects.extend(defer_plugins.then_some(StartPlugins));
                effects
            }
            (Phase::Probing { again, defer_plugins, .. }, LaunchEvent::Landed { reached: Some(false), refused, now }) => {
                let retries = again.map_or(0, |a| a.retries);
                // A server that refuses waits for the reach watcher, which wakes nothing until
                // it accepts; one that accepts but fails its reads tries again on a timer.
                let retry_at = if refused { f64::INFINITY } else { now + retry_delay(retries) };
                *self = Phase::Offline { retries: retries.saturating_add(1), retry_at };
                let wake = (!refused).then_some(WakeAt(retry_at));
                let plugins = defer_plugins.then_some(StartPlugins);
                // A try the app made on its own that misses again changes nothing on screen.
                if again.is_some_and(|a| a.quiet) {
                    let mut effects = Effects::none();
                    effects.extend(wake);
                    effects.extend(plugins);
                    return effects;
                }
                let mut effects = Effects::two(Repaint, Unreachable);
                effects.extend(wake);
                effects.extend(plugins);
                effects
            }
            (phase, _) => {
                *self = phase;
                Effects::none()
            }
        }
    }

    /// No server answered: Home shows the card, a session the unavailable panel.
    pub fn offline(&self) -> bool {
        matches!(self, Phase::Offline { .. } | Phase::Probing { again: Some(_), .. })
    }

    /// The card's button says Checking: a probe the person asked for is on its way.
    pub fn checking(&self) -> bool {
        matches!(self, Phase::Probing { again: None | Some(Again { quiet: false, .. }), .. })
    }

    /// The first reads haven't landed.
    pub fn waiting(&self) -> bool {
        matches!(self, Phase::Waiting { .. } | Phase::Probing { again: None, .. })
    }
}

/// The browser's work that waits for the launch to be over (`engine_fetch`: the sweep of left
/// working copies and the engine's background fetch): owed from `on_ready`, then asked for once.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum EngineFetch {
    #[default]
    NotYet,
    /// `on_ready` ran at `since` (the app's clock, seconds).
    Owed { since: f64 },
    Asked,
}

/// The engine's fetch starts when the window shows its content, or this long after `on_ready`
/// when no server has answered by then.
#[cfg(not(target_arch = "wasm32"))]
const ENGINE_FETCH_LATEST: Duration = Duration::from_secs(2);

/// The saved first screen's write (`saved`): owed from `connect`, written once the window has
/// shown the server's answer for [`SAVED_WRITE_AFTER`]. Building it in `connect` put it in the
/// frame that draws that answer (1.2 ms of that frame's 21.8 on the main thread with 120
/// sessions listed). A window that hides, closes or quits before then writes it at once.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum SavedWrite {
    #[default]
    Idle,
    /// Due at `at` (the app's clock, seconds).
    Owed { at: f64 },
}

/// How long after the server's answer the saved first screen is written.
const SAVED_WRITE_AFTER: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Launch {
    network_change: Option<std::sync::Arc<network::Change>>,
    saved_write: SavedWrite,
    #[cfg(not(target_arch = "wasm32"))]
    engine_fetch: EngineFetch,
    /// A quiet poll for network deliveries; unlike an image completion, the inbox
    /// often contains nothing that changes this window.
    pub inbox_wake: Option<rune::Waker>,
    /// Each server's launch, many at once: every server the window has a half of (`servers::runtime`).
    phases: std::collections::HashMap<ServerId, Phase>,
    /// The restored session whose cached boot-preview messages painted on frame 0 and settle on the
    /// first live message page (`session::pages` or `session::events::incoming`). Independent of
    /// per-server `Phase`: set in `Waiting`, preserved across `Probing` and `Offline`, and still
    /// live in `Connected` until the async first message page lands.
    pub cold_transcript: Option<crate::restore::ColdTranscript>,
    /// The session this window last began reading because its page showed it with nothing reading
    /// its messages (`App::read_shown_session`): asked once while it stays shown, so a failure isn't
    /// retried in a loop (the loading row's Retry asks again). Cleared when the route leaves it or
    /// its messages land.
    pub shown_read: Option<String>,
    /// How long a launch keeps probing the local default address before Home shows the setup
    /// card ([`GRACE`] unless a test sets it): a server that is just starting comes up in this time.
    pub grace: Option<Duration>,
    /// The address the setup card names, in its story.
    pub story_addr: Option<String>,
    /// The restored session's newest page, read beside the first reads, until `connect` hands it
    /// to the session's reader.
    pub early_page: Option<crate::session_pages::Early>,
    /// The session list the first frame drew from the saved state, until the server's lands.
    pub saved_list: Option<saved_list::SavedList>,
    /// What the saved state says of the tab the restored page was in, held until the page's
    /// family is loaded up to its root or the route leaves it (`saved::page_tab`).
    pub page_tab: saved::page_tab::PageTab,
    /// The first reads' thread `setup` started before the first frame, for `on_ready`'s Start.
    #[cfg(not(target_arch = "wasm32"))]
    early: Option<Early>,
}

/// The probe and first reads started in `setup`, beside the window's creation and its first
/// frame, with what they were started for. Started in `on_ready` they began after the first
/// frame and took 12,300 µs of a warm launch (`input.ready` to `server.answered`, a loopback
/// server); `on_ready` takes this thread up instead, with whatever it has already read.
#[cfg(not(target_arch = "wasm32"))]
struct Early {
    rx: Receiver<Ready>,
    accepted: Accepted,
    beta_tabs: bool,
    fallback_fixture: bool,
    grace: Duration,
    /// The window's quiet poll, once `on_ready` has it: the thread wakes nothing before that, so
    /// nothing is polled or drawn ahead of the first frame.
    wake: std::sync::Arc<std::sync::OnceLock<rune::Waker>>,
}

impl Launch {
    pub fn phase(&self, server: &str) -> Option<&Phase> {
        self.phases.get(server)
    }

    pub fn set_phase(&mut self, server: &str, phase: Phase) {
        self.phases.insert(server.to_owned(), phase);
    }

    pub fn forget(&mut self, server: &str) {
        self.phases.remove(server);
    }

    /// Order-independent hash of which servers are currently in [`Phase::Connected`]
    /// (the only `Launch` predicate read by `projects::home::sync`).
    pub(crate) fn connected_stamp(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut count = 0usize;
        let mut xor = 0u64;
        for (id, phase) in &self.phases {
            if matches!(phase, Phase::Connected) {
                count += 1;
                let mut eh = std::collections::hash_map::DefaultHasher::new();
                id.hash(&mut eh);
                xor ^= eh.finish();
            }
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (count, xor).hash(&mut h);
        h.finish()
    }
}

/// A story's window whose server didn't answer; `checking` while a Retry the person pressed is
/// on its way (its answer never comes).
pub(crate) fn story_offline(s: &mut App, checking: bool) {
    let Some(server) = window_server(s) else { return };
    let phase = if checking {
        let (answer, rx) = std::sync::mpsc::channel();
        // The story's probe stays on its way: its sender lives as long as the app.
        std::mem::forget(answer);
        Phase::Probing { rx, again: Some(Again { retries: 0, quiet: false }), accepted: Accepted::default(), defer_plugins: false }
    } else {
        Phase::Offline { retries: 0, retry_at: f64::INFINITY }
    };
    s.launch.set_phase(&server, phase);
}

/// How often the setup card tries again on its own.
fn retry_delay(attempt: u32) -> f64 {
    (0.25 * 2_f64.powi(attempt.min(4) as i32)).min(3.0)
}

pub(crate) fn network_available(s: &mut App, tx: &mut Tx) {
    if !s.live() {
        return;
    }
    Server::wake_reconnects();
    // The engine's background download, when it waits for the network, tries again now.
    #[cfg(not(target_arch = "wasm32"))]
    s.workspace_panes.browser.host.network_changed();
    if let Some(server) = window_server(s) {
        crate::reconnect::step_link(s, &server, opencode_kit::servers::link::Event::NetworkChanged, tx);
    }
    step(s, LaunchEvent::NetworkUp, tx);
    s.pump_outbox();
}

/// `setup`: connects now unless `on_ready` will start the reads (or a synchronous live probe found no server).
pub fn setup(s: &mut App) {
    if !deferred() {
        s.checkout.boot = crate::servers::runtime::Boot::Ready;
    }
    if matches!(window_phase(s), None | Some(Phase::Connected)) {
        start(s, &mut Tx::default());
    }
    // A page asked for the first reads as it started (index.html): its probe takes the answers up
    // now, so when they're in before the first frame, that frame is Home with its sessions.
    #[cfg(target_arch = "wasm32")]
    if matches!(window_phase(s), Some(Phase::Waiting { .. })) {
        start(s, &mut Tx::default());
    }
    // A window: the reads' thread starts now and `on_ready` takes it up (`Early`). The launch
    // itself still starts there, so the first frame is drawn from what it was before.
    #[cfg(not(target_arch = "wasm32"))]
    if let (Some(&Phase::Waiting { beta_tabs, fallback_fixture }), Some(server)) = (window_phase(s), window_server(s)) {
        let grace = probe_grace(s, &server);
        let wake = std::sync::Arc::new(std::sync::OnceLock::<rune::Waker>::new());
        let late = wake.clone();
        let (rx, accepted) = boot::spawn_probe_waking(
            s,
            beta_tabs,
            fallback_fixture,
            grace,
            false,
            Some(std::sync::Arc::new(move || {
                if let Some(wake) = late.get() {
                    wake();
                }
            })),
        );
        s.launch.early = Some(Early { rx, accepted, beta_tabs, fallback_fixture, grace, wake });
    }
}

/// How long `server`'s probe keeps trying: only the local default address may be a server still
/// starting (see `GRACE`).
fn probe_grace(s: &App, server: &str) -> Duration {
    if server == opencode_kit::client::DEFAULT_SERVER || s.launch.grace.is_some() { s.launch.grace.unwrap_or(GRACE) } else { Duration::ZERO }
}

/// The thread `setup` started, when it was started for this probe: its wake is the window's from
/// here on, and one poll takes what it has already sent.
#[cfg(not(target_arch = "wasm32"))]
fn take_early(s: &mut App, beta_tabs: bool, fallback_fixture: bool, grace: Duration, tx: &mut Tx) -> Option<(Receiver<Ready>, Accepted)> {
    let early = s.launch.early.take()?;
    if (early.beta_tabs, early.fallback_fixture, early.grace) != (beta_tabs, fallback_fixture, grace) {
        return None;
    }
    if let Some(wake) = s.launch.inbox_wake.clone().or_else(|| (*s.waker).clone()) {
        early.wake.set(wake).ok();
    }
    tx.wake_after(Duration::ZERO);
    Some((early.rx, early.accepted))
}

/// The server the window works against now (`servers::runtime`'s checkout), keyed as
/// `reconnect::Links` keys it.
pub(crate) fn window_server(s: &App) -> Option<ServerId> {
    Some(crate::servers::runtime::active(s).as_str().to_owned())
}

fn window_phase(s: &App) -> Option<&Phase> {
    s.launch.phase(crate::servers::runtime::active(s).as_str())
}

/// The window's server didn't answer: Home shows the card, a session the unavailable panel.
pub fn offline(s: &App) -> bool {
    window_phase(s).is_some_and(Phase::offline)
}

/// The window's first reads haven't landed.
pub fn waiting(s: &App) -> bool {
    window_phase(s).is_some_and(Phase::waiting)
}

/// The card's Retry says Checking.
pub fn checking(s: &App) -> bool {
    window_phase(s).is_some_and(Phase::checking)
}

/// Coalesce a burst into one quiet poll. An ordinary window wake requests a drawable even
/// when the batch only updated a background session; a timer lets Rune skip that present.
pub fn inbox_waker(handle: rune::AppHandle<App>) -> rune::Waker {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let pending = Arc::new(AtomicBool::new(false));
    Arc::new(move || {
        if pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let pending = pending.clone();
        handle.update_if(move |_, tx| {
            pending.store(false, Ordering::Release);
            tx.wake_after(Duration::ZERO);
            false
        });
    })
}

/// What the browser engine's messages wake: a quiet poll that reads them and steps the agent's
/// browser commands waiting on them, in the turn they arrive. Woken like any other thread, a
/// shown window would poll only in its next frame, so each DevTools round trip of a command
/// waited a frame (17-45 ms as measured); a frame now follows only when the pane changed. The
/// server's announcements of an agent's commands wake it too (`rpc::express`).
#[cfg(not(target_arch = "wasm32"))]
pub fn browser_waker(handle: rune::AppHandle<App>) -> rune::Waker {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let pending = Arc::new(AtomicBool::new(false));
    Arc::new(move || {
        if pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let pending = pending.clone();
        handle.update_if(move |s, tx| {
            pending.store(false, Ordering::Release);
            crate::surfaces::browser::rpc::express::poll(s);
            let changed = s.poll_browser();
            let rpc_changed = crate::surfaces::browser::rpc::poll(s, tx);
            if !changed
                && !rpc_changed
                && let Some(at) = s.workspace_panes.browser.host.scroll_cursor_wake_at()
            {
                tx.wake_after(at.saturating_duration_since(rune::time::Instant::now()));
            }
            changed | rpc_changed
        });
    })
}

/// `on_ready`, after the first frame: starts the probe, Beta tab read and first server reads once.
pub fn ready(s: &mut App, tx: &mut Tx) -> bool {
    if s.live() {
        s.launch.network_change = (*s.waker).clone().and_then(network::subscribe);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(wake) = s.launch.inbox_wake.clone().or_else(|| (*s.waker).clone()) {
            crate::servers::sidecar::register_wake(wake);
        }
    }
    // The dormant built-ins' app menu rows and keys, before the menu bar lists them.
    crate::plugin_host::doze_owed(s);
    // The menu bar was built before the saved shortcuts loaded.
    crate::menubar::refresh();
    crate::updater::start_if_packaged(s);
    // The browser engine, when this build's isn't installed, downloads in the background, and
    // the working copies an import's read left behind are swept: not from here, but once the
    // launch is over (`engine_fetch`).
    #[cfg(not(target_arch = "wasm32"))]
    {
        s.launch.engine_fetch = EngineFetch::Owed { since: *s.clock };
        tx.wake_after(ENGINE_FETCH_LATEST);
    }
    // Which install this is, for the toasts' clicks; it reads the registry, so after the first frame and off the UI thread.
    crate::notifications::settle_identity();
    last_exit::presented();
    if s.persists() {
        crate::windows::reopen::expire(*s.now_ms);
        // Windows an earlier build lost (rows, and no place in the saved list) leave the boot
        // record for Reopen closed window: the first frame is up, and the storage thread does it.
        #[cfg(not(target_arch = "wasm32"))]
        crate::windows::reopen::sweep_lost(*s.now_ms);
    }
    // The window's other servers start from its next poll (`servers::runtime::poll`).
    s.checkout.boot = crate::servers::runtime::Boot::Ready;
    // On the web setup already started the probe, and its first poll may already have
    // connected. Sending Start to Connected would attach workers and prefetch a second time.
    #[cfg(not(target_arch = "wasm32"))]
    start(s, tx);
    // The built-ins the first screen didn't show start now; plugins from folders (`--plugin-dir`,
    // overrides, installed) are read on a worker. Redraw for their panels and commands without
    // waiting for a server response.
    // In a page, after the first reads land (`poll`): the page has one thread, and a built-in's
    // start on it holds the reads' answers back.
    if cfg!(target_arch = "wasm32") && matches!(window_phase(s), Some(Phase::Probing { .. })) {
        step(s, LaunchEvent::DeferPlugins, tx);
        return false;
    }
    let started = crate::plugin_host::first_frame(s);
    if started {
        tx.instant();
    }
    started
}


/// Re-probes the local OpenCode server (including any fresh `service.json` written since launch).
pub fn retry(s: &mut App, tx: &mut Tx) {
    step(s, LaunchEvent::Retry { quiet: false }, tx);
}

pub(crate) fn start(s: &mut App, tx: &mut Tx) {
    let Some(server) = window_server(s) else { return boot::connect(s, tx) };
    // A run that read the server before its first frame has connected.
    if s.launch.phase(&server).is_none() {
        s.launch.set_phase(&server, Phase::Connected);
    }
    step(s, LaunchEvent::Start, tx);
}

/// Steps the window's server's launch and carries out what it asks for.
fn step(s: &mut App, event: LaunchEvent, tx: &mut Tx) -> bool {
    let Some(server) = window_server(s) else { return false };
    let grace = probe_grace(s, &server);
    let Some(mut phase) = s.launch.phases.remove(&server) else { return false };
    let effects = phase.step(event, grace);
    s.launch.set_phase(&server, phase);
    let mut changed = false;
    for effect in effects {
        match effect {
            LaunchEffect::Probe { beta_tabs, fallback_fixture, again, grace } => {
                #[cfg(not(target_arch = "wasm32"))]
                let early = if again.is_none() { take_early(s, beta_tabs, fallback_fixture, grace, tx) } else { None };
                #[cfg(target_arch = "wasm32")]
                let early = None;
                let (rx, accepted) = early.unwrap_or_else(|| boot::spawn_probe(s, beta_tabs, fallback_fixture, grace, again.is_some()));
                s.launch.set_phase(&server, Phase::Probing { rx, again, accepted, defer_plugins: false });
            }
            LaunchEffect::Connect => boot::connect(s, tx),
            LaunchEffect::Unreachable => {
                let now = *s.clock;
                changed |= crate::reconnect::step_link(s, &server, opencode_kit::servers::link::Event::Unreachable { now }, tx);
            }
            LaunchEffect::Fixture => {}
            LaunchEffect::StartPlugins => {
                if crate::plugin_host::first_frame(s) {
                    tx.instant();
                }
            }
            LaunchEffect::WakeAt(at) => tx.wake_after(Duration::from_secs_f64((at - *s.clock).max(0.0))),
            LaunchEffect::Repaint => {
                tx.instant();
                changed = true;
            }
        }
    }
    changed
}

/// `--harness-network-change <secs>`: the OS's network change or wake, stood in for once (`network::simulate`).
fn harness_network_change(s: &App, tx: &mut Tx) {
    static AT: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let Some(at) = *AT.get_or_init(|| crate::arg("--harness-network-change").and_then(|v| v.parse().ok())) else {
        return;
    };
    if DONE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    if *s.clock + 1e-9 < at {
        tx.wake_at(at);
        return;
    }
    DONE.store(true, std::sync::atomic::Ordering::Relaxed);
    rune::marks::mark("harness.network_change");
    network::simulate();
}

/// The launch's first reads have waited `FIRST_PATIENCE` on a server that took their connection:
/// the window shows what it saved and says the server is slow (`Link::Slow`), the same machine
/// that says offline or reconnecting. Woken once for it, when it's due. True when the header changed.
fn first_reads_slow(s: &mut App, tx: &mut Tx) -> bool {
    let Some(Phase::Probing { again: None, accepted, .. }) = window_phase(s) else { return false };
    let Some(at) = accepted.get() else { return false };
    let left = (*at + FIRST_PATIENCE).saturating_duration_since(Instant::now());
    if !left.is_zero() {
        tx.wake_after(left);
        return false;
    }
    let Some(server) = window_server(s) else { return false };
    use opencode_kit::servers::link::{Event, Link};
    if !matches!(crate::reconnect::link_of(s, &server), Link::Online | Link::Connecting) {
        return false;
    }
    let now = *s.clock;
    rune::marks::mark("link.slow");
    crate::reconnect::step_link(s, &server, Event::FirstReadsSlow { now }, tx)
}

/// Starts the browser's work that waits for the launch to be over, once: the loop has waited for
/// input (`input.ready`) and the window shows its content, or `ENGINE_FETCH_LATEST` has passed
/// since `on_ready` with no server answering. Before that nothing of the engine or of an import
/// is touched: no file, no request, no thread.
///
/// - The sweep of the working copies an import's read left behind when the app ended during it
///   (`cookie_import::sweep_left_copies`), on its own thread, whether or not the browser is on:
///   a copy may be older than the choice to turn it off.
/// - The engine's background fetch, the browser's (`browser-pane`): it looks for the installed
///   engine on its own background-priority thread and downloads only when this build's isn't
///   there. A browser turned off (its plugin disabled) fetches nothing, and a run on the mock
///   browser has no engine to fetch.
#[cfg(not(target_arch = "wasm32"))]
fn engine_fetch(s: &mut App) {
    let EngineFetch::Owed { since } = s.launch.engine_fetch else { return };
    // The timer's wake may come a moment early.
    let late = *s.clock - since + 0.05 >= ENGINE_FETCH_LATEST.as_secs_f64();
    // A headless run (a harness's realtime probe of the engine) has no loop that waits for input.
    let input_ready = crate::headless() || rune::marks::marks().iter().any(|(name, _)| *name == "input.ready");
    if (waiting(s) && !late) || !input_ready {
        return;
    }
    s.launch.engine_fetch = EngineFetch::Asked;
    crate::surfaces::browser::cookie_import::sweep_left_copies();
    if !s.use_mock_browser() && s.plugins.plugin_available(crate::surfaces::browser::PLUGIN) {
        s.workspace_panes.browser.host.fetch_in_background();
    }
}

/// `poll`: the launch's first reads (`first_reads`), then the browser engine's background fetch
/// once the launch is over.
pub fn poll(s: &mut App, tx: &mut Tx) -> bool {
    let changed = first_reads(s, tx);
    #[cfg(not(target_arch = "wasm32"))]
    engine_fetch(s);
    // The launch's other saved windows, one per turn once this one takes input.
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(after) = crate::windows::open_rest(s) {
        tx.wake_after(after);
    }
    saved_write(s);
    changed
}

/// Writes the saved first screen once it's due (`SavedWrite`): what the server lists now, for
/// the next launch's first frame.
fn saved_write(s: &mut App) {
    let SavedWrite::Owed { at } = s.launch.saved_write else { return };
    // The timer's wake may come a moment early.
    if *s.clock + 0.05 < at {
        return;
    }
    s.launch.saved_write = SavedWrite::Idle;
    saved_list::save(s);
    saved_catalog::save(s);
    saved_gutters::save(s);
}

/// The first reads land at once (nothing animates in on the first load), then connect.
fn first_reads(s: &mut App, tx: &mut Tx) -> bool {
    if s.launch.network_change.as_ref().is_some_and(|n| n.take()) {
        network_available(s, tx);
    }
    harness_network_change(s, tx);
    last_exit::poll_debug_drill();
    let now = *s.clock;
    if s.live() && crate::attended() {
        if window_phase(s).is_some_and(|p| matches!(p, Phase::Offline { retry_at, .. } if now + 1e-3 >= *retry_at)) {
            step(s, LaunchEvent::Tick { now }, tx);
        }
        if window_phase(s).is_some_and(|p| matches!(p, Phase::Offline { retry_at, .. } if retry_at.is_infinite()))
            && matches!(
                crate::servers::health::get(crate::servers::runtime::active(s)),
                Some(opencode_servers::health::Health::Probe(opencode_servers::health::ProbeResult::Healthy { .. }))
            )
        {
            step(s, LaunchEvent::Answering, tx);
        }
    }
    let slow = first_reads_slow(s, tx);
    // A server that took the connection after the state was read: the saved list stands in from here.
    let stood_in = matches!(window_phase(s), Some(Phase::Probing { .. })) && saved_list::restore(s);
    if stood_in {
        tx.instant();
    }
    let slow = slow || stood_in;
    // Every poll comes here: the window's key is borrowed, not copied.
    let Some(Phase::Probing { rx, .. }) = window_phase(s) else { return slow };
    let ready = match rx.try_recv() {
        Ok(r) => r,
        Err(TryRecvError::Empty) => return slow,
        Err(TryRecvError::Disconnected) => Ready {
            server: None,
            beta_tabs: None,
            first: Some(Err("The first server reads stopped".into())),
            refused: false,
            page: None,
        },
    };
    let reached = ready.first.as_ref().map(Result::is_ok);
    let refused = ready.refused;
    s.launch.early_page = ready.page;
    // A launch that reached nobody must not read as one whose server answered: the reads that
    // failed (no connection, a refused or failed TLS handshake, a bad answer) or found no server
    // are their own mark.
    rune::marks::mark(if reached == Some(true) { "server.answered" } else { "server.failed" });
    tx.instant();
    let Some(result) = ready.first else {
        step(s, LaunchEvent::Landed { reached, refused, now }, tx);
        s.launch.cold_transcript = None;
        crate::windows::peer::leave_server(s);
        s.transcripts.clear();
        s.tabs.clear();
        *s.route = Route::Home;
        s.load_fixture(crate::FIXTURE);
        return true;
    };
    if let (Some(actual), Source::Live { server, .. }) = (ready.server, &mut *s.source) {
        *server = actual.clone();
        if crate::servers::runtime::active(s).is_sidecar() {
            crate::servers::runtime::moved(s, &actual);
        }
    }
    if let Some(tabs) = ready.beta_tabs {
        let drafts: Vec<_> = s.tabs.drain(..).filter(crate::tabs::Tab::is_draft).collect();
        *s.tabs = tabs;
        s.tabs.extend(drafts);
    }
    match result {
        Ok(first) => {
            s.apply_first(first);
            rune::marks::mark("server.first_reads");
            step(s, LaunchEvent::Landed { reached, refused, now }, tx);
            s.show_outbox();
            s.pump_outbox();
            true
        }
        // The tabs and the page stay, as Desktop never drops a tab while its server is away
        // (`packages/app/src/shell/tabs/tabs.tsx`): Home shows the card, a session whose server
        // hasn't answered the unavailable panel (`view::session_state`).
        Err(e) => {
            let shown = step(s, LaunchEvent::Landed { reached, refused, now }, tx);
            if shown {
                s.diag.status = e.into();
            }
            shown
        }
    }
}
