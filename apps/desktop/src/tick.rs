//! Four-phase per-frame tick (`poll`), declared scheduler stages (`stage`), one-time window
//! `setup`, and `visibility` lifecycle hooks.
//!
//! Decoupled feature and subsystem polls run through `s.kernel.scheduler` in explicit phases
//! (`Ingest`, `Derive`, `React`, `Persist`), ordered by declared [`stage`] barriers and explicit
//! `after` edges independently of feature registration order.

use opencode_kernel::{Phase, Stage};
use opencode_kit::model::Delivery;
use rune::{Tx, Waker};

use crate::{
    App, Route, demo, headless, incoming, launch, plugin_host, stop, storage, submit, telemetry,
    wall_ms, windows,
};

/// Declared execution stages within each scheduler phase, in execution order.
pub(crate) mod stage {
    use super::{Phase, Stage};

    // ── Phase 1: Ingest ──────────────────────────────────────────────────────────────────────────
    /// Pre-server storage hydration, peer-dirty check, pre-stream restore/audit, popup settle.
    pub const INGEST_HYDRATE: Stage = Stage::new(Phase::Ingest, "ingest.hydrate");
    /// Server checkout/background, health probes, Home/project-switcher sync, dictation, launch.
    pub const INGEST_SERVERS: Stage = Stage::new(Phase::Ingest, "ingest.servers");
    /// Local OS/appearance/language/surface workers and settings timers.
    pub const INGEST_LOCAL: Stage = Stage::new(Phase::Ingest, "ingest.local");
    /// Notification clicks, screen-reader speech, composer-focus sync, deep-link drain.
    pub const INGEST_DISPATCH: Stage = Stage::new(Phase::Ingest, "ingest.dispatch");
    /// Pre-page/pre-stream session input and indicator polls.
    pub const INGEST_SESSION_PRE: Stage = Stage::new(Phase::Ingest, "ingest.session_pre");
    /// Page loads, context window, inbox send receipts/outbox, output errors, open-in, jobs.
    pub const INGEST_PAGES: Stage = Stage::new(Phase::Ingest, "ingest.pages");
    /// Live SSE/WS & fixture playback stream drain, post-stream history timers, held submits.
    pub const INGEST_STREAM: Stage = Stage::new(Phase::Ingest, "ingest.stream");

    // ── Phase 2: Derive ──────────────────────────────────────────────────────────────────────────
    /// Timeline disclosures, projections, animations, find, pickers, worktrees.
    pub const DERIVE_TIMELINE: Stage = Stage::new(Phase::Derive, "derive.timeline");
    /// Home session search and command/session switcher.
    pub const DERIVE_SEARCH: Stage = Stage::new(Phase::Derive, "derive.search");
    /// Composer completions/suggestions, session menu rename/delete, branch picker, uploads.
    pub const DERIVE_COMPOSER: Stage = Stage::new(Phase::Derive, "derive.composer");

    // ── Phase 3: React ───────────────────────────────────────────────────────────────────────────
    /// Attachment polling, side/bottom panels & plugins, MCP tool sync, turn diffs.
    pub const REACT_PANELS: Stage = Stage::new(Phase::React, "react.panels");
    /// Settings page reactions, server reconnect/session resolution, tips, status toasts.
    pub const REACT_STATUS: Stage = Stage::new(Phase::React, "react.status");
    /// Layout/narrow transitions, sidebar inbox, tab preview, pending-request reads, draft swap.
    pub const REACT_SHELL: Stage = Stage::new(Phase::React, "react.shell");

    // ── Phase 4: Persist ─────────────────────────────────────────────────────────────────────────
    /// Startup profile lock, restore hydration/persistence, saved comments, window/OS persistence.
    pub const PERSIST_FLUSH: Stage = Stage::new(Phase::Persist, "persist.flush");

    /// All desktop scheduler stages in execution order.
    pub const ALL: &[Stage] = &[
        INGEST_HYDRATE,
        INGEST_SERVERS,
        INGEST_LOCAL,
        INGEST_DISPATCH,
        INGEST_SESSION_PRE,
        INGEST_PAGES,
        INGEST_STREAM,
        DERIVE_TIMELINE,
        DERIVE_SEARCH,
        DERIVE_COMPOSER,
        REACT_PANELS,
        REACT_STATUS,
        REACT_SHELL,
        PERSIST_FLUSH,
    ];
}

/// Per-frame tick handoff slot carrying the pre-stream `dock_face` snapshot from `tick.stream`
/// into Derive's `timeline.dock_morph`.
#[derive(Default)]
struct TickFrame {
    asked_dock_face: Option<String>,
}

static TICK_FRAME: opencode_kernel::Slot<TickFrame> = opencode_kernel::Slot::new();

impl App {
    pub(crate) fn take_asked_dock_face(&mut self) -> Option<String> {
        self.kernel.slots.get_or_default(&TICK_FRAME).asked_dock_face.take()
    }
}

/// Snapshots `dock_face`, drains live SSE/WS events and due fixture playback events, and runs
/// `stop::poll` at the exact mid-ingest boundary (`31a59a9:apps/desktop/src/main.rs`).
pub(crate) fn poll_stream(s: &mut App, now: f64, tx: &mut Tx) -> bool {
    let asked = s.dock_face();
    s.kernel.slots.get_or_default(&TICK_FRAME).asked_dock_face = asked;
    let mut changed = incoming::poll(s, tx);
    while s.fixture_run.playback.front().is_some_and(|e| e.0 <= now + 1e-9) {
        let Some((at, ty, mut data)) = s.fixture_run.playback.pop_front() else { break };
        if ty.starts_with("session.execution.") || ty == "session.archived" {
            data["eventCreated"] = (*s.fixture_run.base_ms + (at * 1000.0) as i64).into();
        }
        // A scripted delivery names the oldest prompt of its kind, which has no id until sent.
        if let Some(kind) = Delivery::parse(&data["next"])
            && let Some(item) = s
                .inboxes
                .get(data["sessionID"].as_str().unwrap_or(""))
                .and_then(|i| i.items.iter().find(|i| i.delivery == kind))
        {
            data["inboxID"] = item.id.clone().into();
        }
        if ty.starts_with("demo.") {
            demo::apply(s, &ty, &data, tx);
            changed = true;
            continue;
        }
        if ty == "fixture.page" {
            s.show_fixture_page(data["sessionID"].as_str().unwrap_or(""), tx);
            changed = true;
            continue;
        }
        if let Some(sid) = data["sessionID"]
            .as_str()
            .filter(|sid| ty == "session.step.started" && s.route == Route::here(sid.to_string()))
        {
            s.refresh_fixture_page(sid);
        }
        s.live_event(ty, data);
        changed = true;
    }
    // The next playback event is due by the clock: the window frames then too (not only when the
    // playback thread's wake arrives, which may be a frame later), as a timer asks.
    if let Some(&(at, _, _)) = s.fixture_run.playback.front() {
        tx.wake_at(at);
    }
    changed |= stop::poll(s, now, tx);
    changed
}

/// Arms queue/start timers and runs `submit::poll` right after rewind/fork/jump polls.
pub(crate) fn poll_submit(s: &mut App, tx: &mut Tx) -> bool {
    s.queue_timers(tx);
    s.start_timers(tx);
    submit::poll(s, tx)
}

pub(crate) fn poll(s: &mut App, now: f64, tx: &mut Tx) -> bool {
    session_view::scroller::set_device_scale(windows::view_scale(s));
    *s.clock = now;
    s.sync_history();
    *s.now_ms = if s.live() { wall_ms() } else { *s.fixture_run.base_ms + (now * 1000.0) as i64 };
    demo::poll(s);
    // The morph loop is for watching: the composer it lands on doesn't take the keys.
    if s.fixture_run.stage.is_some_and(|st| st.watch()) {
        s.composer.focused = false;
    }
    let scheduler = s.kernel.scheduler.clone();
    crate::panels::state::flight::begin_poll(s);
    let changed = scheduler.run_all(s, now, tx);
    crate::shell::memory_reclaim::poll(s, tx, changed);
    s.finish_tail_change(tx);
    crate::panels::state::flight::end_poll(s, tx);
    changed
}

pub(crate) fn setup(s: &mut App, waker: Waker) {
    *s.waker = Some(waker.clone());
    storage::on_ready(waker.clone());
    // The outbox was opened before the window had a waker: its stored prompts wake it when they're in.
    s.listen_outbox();
    windows::peer::set_waker(s, waker.clone());
    s.appearance_state.icons.set_waker(waker.clone());
    s.turn_diffs.set_waker(waker.clone());
    s.kernel.slots.get(&crate::app::MATH).set_waker(waker.clone());
    s.appearance_state.icons.set_sync(headless());
    s.setup_browser(waker.clone());
    rune::marks::mark("browser.setup");
    // Only the plugins this launch's first screen shows start before its first frame (`first_screen`).
    telemetry::startup("launch.plugins", || plugin_host::setup(s, waker.clone()));
    rune::marks::mark("plugins.started");
    let scheduler = s.kernel.scheduler.clone();
    scheduler.run_setup(s, waker);
    if !s.fixture_run.playback.is_empty() {
        s.wake_at_playback();
    }
    launch::setup(s);
}

/// A window shown or hidden: hidden, what it changed lands at once, without waiting for it
/// (Desktop flushes its stores on `visibilitychange` to hidden, `renderer/platform/storage.ts`).
pub(crate) fn visibility(s: &mut App, shown: bool, tx: &mut Tx) {
    let scheduler = s.kernel.scheduler.clone();
    scheduler.run_visibility(s, shown, tx);
    if !shown && s.persists() {
        s.poll_restore();
        s.poll_preview(true);
        launch::saved_list::save(s);
        launch::saved_catalog::save(s);
        launch::saved_gutters::save(s);
        storage::flush(opencode_storage::FlushReason::Hide, false);
    }
}
