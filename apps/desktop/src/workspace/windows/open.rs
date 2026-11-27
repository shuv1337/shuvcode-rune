//! Opening windows: every window saved at the last quit at launch (Desktop's `restore`), and a
//! new one in the running app (`window.new`, a torn-out tab).

use std::collections::HashMap;

use session_view::composer::mode::Mode;

use rune::{AppSpec, Display, WindowHandle, WindowSpec};

use super::peer;
use super::store::{self, Frame};
use super::{Window, register};
use crate::tabs::Tab;
use crate::{App, Route};

/// What a window opened from another starts with: the tabs it moves, each with its server, and
/// their drafts. It shows them at once from their servers' shared copies (`servers::runtime`).
pub struct Seed {
    pub tabs: Vec<Tab>,
    pub route: Route,
    pub drafts: HashMap<String, String>,
    /// The moving drafts' attachments, quotes and pills.
    parked: HashMap<String, crate::restore::Parked>,
    /// The moving drafts in shell mode, which they keep.
    shell: Vec<String>,
    default_dir: String,
}

impl Seed {
    /// `tabs` (with their drafts) moving out of `s` into the new window, `route` showing, in
    /// `s`'s session data.
    pub fn from(s: &App, tabs: Vec<Tab>, route: Route) -> Seed {
        Seed {
            drafts: tabs.iter().filter_map(|t| Some((t.id.clone(), s.draft_text(&t.id)?))).collect(),
            parked: tabs.iter().filter_map(|t| Some((t.id.clone(), s.draft_parked(&t.id)?))).collect(),
            shell: tabs.iter().filter(|t| s.draft_mode(&t.id) == Mode::Shell).map(|t| t.id.clone()).collect(),
            tabs,
            route,
            default_dir: (*s.default_dir).clone(),
        }
    }
}

impl App {
    /// The window's first state: its saved tabs and page, then what its opener gave it.
    pub(crate) fn new_in(mut w: Window) -> App {
        let seed = w.seed.take();
        let mut app = App::new_window(w);
        if let Some(seed) = seed {
            app.plant(seed);
        }
        app
    }

    fn plant(&mut self, seed: Seed) {
        *self.default_dir = seed.default_dir;
        *self.tabs = seed.tabs;
        let r = self.kernel.slots.get_mut(&crate::restore::RESTORE);
        r.drafts.extend(seed.drafts);
        r.parked.extend(seed.parked);
        for id in seed.shell {
            self.kernel.slots.get_mut(&crate::add_menu::ADD_MENU).park(id, Mode::Shell);
        }
        *self.route = seed.route;
        // Before the window's first turn: the page's server's copy comes in for this, and goes back.
        peer::in_turn(self, |s| {
            s.set_media_fetch();
            s.show_draft();
            s.save_tabs();
        });
    }

    /// Draft `id`'s mode, the open one's from the composer.
    pub(crate) fn draft_mode(&self, id: &str) -> Mode {
        if self.kernel.slots.get(&crate::restore::RESTORE).shown.as_deref() == Some(id) {
            return self.kernel.slots.get(&crate::add_menu::ADD_MENU).mode;
        }
        self.kernel.slots.get(&crate::add_menu::ADD_MENU).parked().find(|(k, _)| *k == id).map_or(Mode::Normal, |(_, m)| m)
    }

    /// The composer words kept for tab `id`, the open one's from the composer.
    pub(crate) fn draft_text(&self, id: &str) -> Option<String> {
        let r = self.kernel.slots.get(&crate::restore::RESTORE);
        let text = if r.shown.as_deref() == Some(id) { Some(self.composer.text.clone()) } else { r.drafts.get(id).cloned() };
        text.filter(|t| !t.trim().is_empty())
    }
}

/// How many windows the launch opened (`launch`), for the launch trace at quit.
#[cfg(not(target_arch = "wasm32"))]
static OPENED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(not(target_arch = "wasm32"))]
pub fn opened() -> usize {
    OPENED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The displays the launch found, which a window reopened later fits its frame to (Rune tells
/// the displays only to the launch).
static DISPLAYS: std::sync::Mutex<Vec<Display>> = std::sync::Mutex::new(Vec::new());

/// Where a window saved at `f` opens now: [`super::placement::fit`] on the launch's displays,
/// or `f` as it is without them.
pub(super) fn refit(f: Frame) -> Option<Frame> {
    let displays = DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
    if displays.is_empty() {
        return Some(f);
    }
    super::placement::fit(f, &displays)
}

/// Scale of the display `frame` opens on, else the primary display's, before the OS window exists
/// (or on Wayland, where `WindowHandle::placement` hides positions).
fn display_scale(frame: Option<&Frame>) -> Option<f32> {
    let displays = DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
    let bounds = |d: &Display| [d.position.0, d.position.1, d.size.0 as i32, d.size.1 as i32];
    let by_frame = frame.and_then(|f| {
        let by_pos = f.x.zip(f.y).and_then(|(x, y)| {
            let (cx, cy) = (x + f.width as i32 / 2, y + f.height as i32 / 2);
            displays.iter().find(|d| {
                let b = bounds(d);
                cx >= b[0] && cy >= b[1] && cx < b[0] + b[2] && cy < b[1] + b[3]
            })
        });
        by_pos.or_else(|| f.display.and_then(|saved| displays.iter().find(|d| bounds(d) == saved)))
    });
    by_frame.or_else(|| displays.iter().find(|d| d.primary)).or_else(|| displays.first()).map(|d| d.scale as f32)
}

/// The windows saved at the last quit after the first, with where each opens: [`open_rest`]
/// opens them once the first one takes input.
#[cfg(not(target_arch = "wasm32"))]
static REST: std::sync::Mutex<Vec<(String, Option<Frame>)>> = std::sync::Mutex::new(Vec::new());

/// Opens the launch's next saved window after the first, once the loop has waited for input
/// (`input.ready`): one per turn. Rune builds every window it is given at launch, and draws each
/// one's first frame, before the first of them takes a key (two saved windows: input 65-76 ms
/// after `main`, against 42-50 with one; strobe `rec_74d5a1`, `rec_4b5e43`, `rec_b46005`), so the launch gives
/// it the first window alone. Answers when to look again while one is left to open: a moment
/// later while the loop hasn't waited yet (a wake at once would keep it from waiting), else at
/// the next turn.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_rest(s: &App) -> Option<std::time::Duration> {
    let next = {
        let mut rest = REST.lock().unwrap_or_else(|e| e.into_inner());
        if rest.is_empty() {
            return None;
        }
        if !rune::marks::marks().iter().any(|(name, _)| *name == "input.ready") {
            return Some(std::time::Duration::from_millis(2));
        }
        rest.remove(0)
    };
    rune::marks::mark("window.rest.open");
    let _ = rune::open_window(window(Window::new(next.0, false), next.1, &s.diag.diagnostics));
    (!REST.lock().unwrap_or_else(|e| e.into_inner()).is_empty()).then_some(std::time::Duration::ZERO)
}

/// Opens the first window saved at the last quit, or one for a new profile, where it was, and
/// leaves the other saved windows to [`open_rest`]. The list and the frames come from the boot
/// packet, taken once the event loop is up (so the storage's read runs under it); a list that
/// didn't change isn't written again (`store::launch_ids`). Returns when the app quits, unless
/// it ends at once (`rune::fast_exit`).
#[cfg(not(target_arch = "wasm32"))]
pub fn launch(diagnostics: crate::diagnostics::Diagnostics) {
    rune::run_windows(move |displays: &[Display]| {
        *DISPLAYS.lock().unwrap_or_else(|e| e.into_inner()) = displays.to_vec();
        let (ids, fresh) = store::launch_ids();
        OPENED.store(ids.len(), std::sync::atomic::Ordering::Relaxed);
        // Nothing saved (a new profile): where Desktop's first window opens.
        let frame = |id: &str| store::load_frame(id).and_then(|f| super::placement::fit(f, displays)).or_else(|| super::placement::first(displays));
        *REST.lock().unwrap_or_else(|e| e.into_inner()) = ids.iter().skip(1).map(|id| (id.clone(), frame(id))).collect();
        vec![window(Window::new(ids[0].clone(), fresh), frame(&ids[0]), &diagnostics).boxed()]
    });
}

/// New Window: another window of this app, on the same server, showing Home (Desktop's
/// `window.new`, a fresh window id with nothing saved).
pub fn new_window(s: &App) -> Option<WindowHandle> {
    open(s, Window::new(store::new_id(), false), None, false)
}

/// Opens `w` beside the open ones: at `frame` (else where the system puts a new window), and
/// `inactive`, without taking focus from the window the user works in.
pub(super) fn open(s: &App, w: Window, frame: Option<Frame>, inactive: bool) -> Option<WindowHandle> {
    if !s.persists() || s.window.id.is_empty() {
        return None;
    }
    // A torn-out tab's window joins the saved list only if it stays (`life`).
    if w.life.saved() {
        let mut ids = super::saved_ids();
        ids.push(w.id.clone());
        store::save_ids(&ids);
    }
    let spec = window(w, frame, &s.diag.diagnostics);
    Some(rune::open_window(if inactive { spec.inactive() } else { spec }))
}

/// Window `w`'s spec: the app, at `frame` when it has one.
fn window(mut w: Window, frame: Option<Frame>, diagnostics: &crate::diagnostics::Diagnostics) -> WindowSpec<App> {
    w.placement = super::placement::Placement::opening(frame.as_ref());
    w.scale = display_scale(frame.as_ref()).unwrap_or(0.0);
    let (id, life) = (w.id.clone(), w.life);
    // What this window's view read from layout when it last rested or closed: its first view
    // is given the same rects, and runs once when the layout agrees with them.
    let layout = life.saved().then(|| (store::load_layout(&id), id.clone()));
    let diagnostics = diagnostics.clone();
    let app_diagnostics = diagnostics.clone();
    let app = crate::rune_app(AppSpec { families: crate::appearance::launch_families(), ..crate::spec(App::new) }).init(move || {
        let mut app = App::new_in(w);
        *app.diag.diagnostics = app_diagnostics;
        app
    });
    // The saved layout's titlebar before the window exists: the first frame is drawn once.
    #[cfg(target_os = "macos")]
    let app = app.titlebar(crate::view::saved_mac_titlebar());
    let handle = app.handle();
    let ready = handle.clone();
    let mut motion_log = crate::motion_log::Log::default();
    let (encoded, drawn) = (diagnostics.clone(), diagnostics);
    let display_hz = std::rc::Rc::new(std::cell::Cell::new(None::<f64>));
    let display_hz_enc = display_hz.clone();
    // The first frame draws with the appearance the last run saw; the portal's answer follows.
    #[cfg(target_os = "linux")]
    rune::last_appearance(store::load_appearance(), store::save_appearance);
    let app = match layout {
        Some((seed, id)) => app.layout_seed(seed, move |seed| store::save_layout(&id, &seed)),
        None => app,
    };
    let app = app
        .on_ready(move |_| {
            // The first frame is up: the storage's second stage is taken off the UI thread.
            crate::storage::presented();
            // Stream batches wake the window for a quiet poll, which draws only what they change.
            let wake = crate::launch::inbox_waker(ready.clone());
            // The web has no browser engine: nothing for its messages to wake.
            #[cfg(not(target_arch = "wasm32"))]
            let browser = crate::launch::browser_waker(ready.clone());
            ready.update_if(move |s, tx| {
                s.launch.inbox_wake = Some(wake);
                #[cfg(not(target_arch = "wasm32"))]
                s.workspace_panes.browser.host.set_message_waker(browser);
                crate::launch::ready(s, tx)
            })
        })
        .on_frame_encoded(move |f| {
            if let Some(hz) = f.refresh_hz() { display_hz_enc.set(Some(hz)); }
            encoded.encoded(f);
            crate::telemetry::encoded(f);
            crate::files::trace::frame(f);
        })
        .on_frame_drawn(move |rt| {
            if let Some(hz) = display_hz.get() { rt.state.workspace_panes.browser.host.set_display_hz(hz); }
            drawn.drawn(rt);
            if let Some((sid, msgs)) = rt.state.painted_session()
                && msgs > 0
                && !crate::launch::waiting(&rt.state)
                && rt.state.launch.cold_transcript.is_none()
                && rt.state.pages.first_read_done(sid)
            {
                crate::launch::last_exit::mark_restore_settled(sid);
            }
            if let Some(frame) = crate::telemetry::last_frame() {
                crate::telemetry::drawn(rt.state.painted_session(), frame);
                crate::telemetry::stream_drawn(&rt.state.window.id, frame.0);
            }
            crate::launch::trace::drawn(rt);
            motion_log.frame(rt);
        })
        // Back on screen (the Dock icon, the Window menu): no longer the hidden last window.
        .on_focus(|s, focused, _| {
            s.window.hidden &= !focused;
            s.window.focused = focused;
            if focused {
                super::focused(&s.window.id);
                s.taskbar_focused();
            }
        })
        // Quitting (File > Quit, the Quit command) installs a ready update on the way out too, as
        // closing the last window does; the second call of the two does nothing.
        .on_quit(|s, _| {
            // Before the state is saved: the engine's download has that long to leave its write.
            #[cfg(not(target_arch = "wasm32"))]
            browser_pane::installer::quit();
            s.plugins.restarts.cancel_all();
            let scheduler = s.kernel.scheduler.clone();
            scheduler.run_close(s);
            s.save_on_quit();
        })
        .on_close(|s, _| {
            s.plugins.restarts.cancel_all();
            let scheduler = s.kernel.scheduler.clone();
            scheduler.run_close(s);
            s.save_on_close();
        })
        .on_rest(crate::App::rest);
    let mut spec = WindowSpec::from_app(format!("window-{id}"), app);
    // A harness's launch on the visible desktop: a process the user didn't start can't come to
    // the front, so its window would open behind theirs, covered, and stop drawing. Above every
    // window instead, without taking the focus.
    spec.options.always_on_top = crate::flag("--launch-trace-on-top");
    if let Some(f) = frame {
        spec.options.position = f.x.zip(f.y);
        spec.options.physical_size = Some((f.width, f.height));
        spec = spec.maximized(f.maximized).full_screen(f.full_screen);
    }
    spec.on_created(move |native| register(&id, native, handle, life))
}
