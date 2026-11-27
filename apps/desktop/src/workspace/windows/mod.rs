//! The app's windows, all in this process as Desktop's are in its main process
//! (`packages/desktop/src/main/windows`): each has its own tabs, page and layout, and they share
//! the server connection's event stream (`opencode_kit::client::EventHub`) and one copy of the
//! session data it brings (`peer`), and the plugins' app halves (`plugin_host`).
//!
//! New Window (Ctrl+Shift+N, the ≡ menu) opens one with Rune's `open_window`; Close Window
//! (Ctrl+Shift+W) closes the one in front (`close`). Each keeps its tabs, closed tabs, panes, page and
//! drafts under its own id (`store`), and every window open at quit opens again where it was.
//! A tab dragged off the strip opens in a window of its own under the pointer (`tear`).

use std::cell::RefCell;
use std::sync::mpsc::{Receiver, Sender};

use opencode_kit::client::{Incoming, Request, Write};
use rune::{AppHandle, WindowHandle};

use crate::App;

pub mod close;
mod life;
mod open;
pub mod peer;
pub mod placement;
pub mod reopen;
pub mod store;
pub mod taskbar;
pub mod tear;
pub(crate) mod tear_stories;

pub use open::new_window;
#[cfg(not(target_arch = "wasm32"))]
pub use open::{launch, open_rest, opened};
pub use reopen::reopen;

/// One window's identity and what it tracks of itself.
#[derive(Default)]
pub struct Window {
    /// Its persistent id (`opencode.window.<id>.dat`); empty for headless runs and tests, which
    /// keep nothing.
    pub id: String,
    /// The one window of a profile that never saved a window list: Desktop's tabs come into it
    /// (`crate::seed`).
    pub fresh: bool,
    /// Where it is, and the frame restoring it returns to.
    pub placement: placement::Placement,
    /// When `placement` was last read (`App::clock`).
    read_at: std::cell::Cell<f64>,
    /// What its taskbar button shows.
    pub taskbar: taskbar::Taskbar,
    /// The frame last saved.
    saved: Option<store::Frame>,
    /// The tab strip's rect in window points, where a tab dropped from another window lands.
    pub strip: Option<[f32; 4]>,
    /// A tab being dragged out of this window's strip into a window of its own.
    pub tear: tear::Tear,
    /// Tabs the window opened with (a torn-out tab), and their drafts.
    pub seed: Option<open::Seed>,
    /// How it starts its life: open, or a torn-out tab's window following the pointer.
    pub life: life::Life,
    /// The last window, hidden by Close Window on macOS while the app keeps running.
    pub hidden: bool,
    /// Its display scale before or outside `view` (from the opening display, `placement`, or web).
    pub scale: f32,
    /// It holds the keyboard (`on_focus`): only then does it report what's seen as viewed.
    pub focused: bool,
}

impl Window {
    /// A window with its own rows.
    pub fn new(id: String, fresh: bool) -> Window {
        Window { id, fresh, ..Window::default() }
    }

    /// A window that keeps nothing: headless runs and tests.
    pub fn unsaved() -> Window {
        Window::default()
    }
}

/// The profile's first saved window (a new one when none was saved), so a headless harness run
/// that writes (`--harness-writes`) reads and writes the rows the app's first window keeps
/// (`opencode.window.<id>.dat`): its tabs, page and drafts.
pub fn first_saved() -> Window {
    let (mut ids, fresh) = store::launch_ids();
    Window::new(ids.swap_remove(0), fresh)
}

/// An open window: its handles, where its strip is, and its life.
struct Open {
    id: String,
    handle: WindowHandle,
    app: AppHandle<App>,
    strip: Option<[f32; 4]>,
    life: life::Life,
    /// When it last came to the front (`FOCUSES`): of windows under one point, the latest is on top.
    focused: u64,
    /// False between `register` and this window's first `sync_shown` (`servers::runtime::leave`):
    /// while unreported, another window's `App::rest` treats this window as showing everything so
    /// shared transcripts and user-message metadata are never evicted in the startup gap.
    reported: bool,
    /// Stamp of `(tabs, route)` when `shown` was last refreshed.
    shown_stamp: u64,
    /// Every `(server, session_id)` this window holds in its tabs or active route, so another
    /// window resting (`App::rest`) never evicts a session still open here.
    shown: Vec<(opencode_kit::servers::ServerKey, String)>,
    /// Weak handles to every `Pages` prefetch cache in this window (active and parked servers)
    /// so another window's `App::rest` never prunes user-message metadata for pages cached here.
    pages: Vec<crate::session_pages::CacheRef>,
}

thread_local! {
    /// Every open window, in the order they opened (the UI thread's, which all windows share),
    /// and a window kept at its close while the app ends.
    static OPEN: RefCell<Vec<Open>> = const { RefCell::new(Vec::new()) };
    /// Counts windows coming to the front.
    static FOCUSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static QUIT: RefCell<QuitSave> = const { RefCell::new(QuitSave::Idle) };
}

/// The quit's save: Rune runs `on_quit` in every open window, one after another; each queues
/// what it keeps, and the last lands all of it in one transaction (a Windows session may end
/// the process once the hooks returned).
#[derive(Debug, Default, PartialEq, Eq)]
enum QuitSave {
    #[default]
    Idle,
    /// The windows whose `on_quit` hasn't run yet.
    Saving { left: Vec<String> },
}

enum QuitEvent {
    /// Window `id`'s `on_quit` queued its rows; `open` are the windows open now.
    Saved { id: String, open: Vec<String> },
}

#[derive(Debug, PartialEq, Eq)]
enum QuitEffect {
    None,
    /// Every window queued: land it all.
    Flush,
}

impl QuitSave {
    fn step(self, event: QuitEvent) -> (QuitSave, QuitEffect) {
        let QuitEvent::Saved { id, open } = event;
        let left: Vec<String> = match self {
            QuitSave::Idle => open.into_iter().filter(|w| *w != id).collect(),
            QuitSave::Saving { left } => left.into_iter().filter(|w| *w != id && open.contains(w)).collect(),
        };
        if left.is_empty() {
            return (QuitSave::Idle, QuitEffect::Flush);
        }
        (QuitSave::Saving { left }, QuitEffect::None)
    }
}

/// Window `id` queued what it keeps for the quit; the last window to do so lands every window's
/// rows in one transaction and waits for it.
pub fn quit_saved(id: &str) {
    let open = OPEN.with(|o| o.borrow().iter().map(|w| w.id.clone()).collect());
    let effect = QUIT.with(|q| {
        let state = std::mem::take(&mut *q.borrow_mut());
        let (next, effect) = state.step(QuitEvent::Saved { id: id.to_owned(), open });
        *q.borrow_mut() = next;
        effect
    });
    if effect == QuitEffect::Flush {
        crate::storage::flush(opencode_storage::FlushReason::Quit, true);
    }
}

/// Window `id` is on screen: other windows can reach it from now on.
pub fn register(id: &str, handle: WindowHandle, app: AppHandle<App>, life: life::Life) {
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        o.retain(|w| w.id != id);
        o.push(Open { id: id.to_owned(), handle, app, strip: None, life, focused: 0, reported: false, shown_stamp: 0, shown: Vec::new(), pages: Vec::new() });
    });
    focused(id);
}

/// Records the `(server, session_id)` entries this window holds in its open tabs and active route
/// (plus its `Pages` prefetch cache handle), so another window's `App::rest` never evicts sessions
/// or user-message metadata still live here.
pub fn sync_shown(s: &App) {
    if s.window.id.is_empty() {
        return;
    }
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    rune::Tracked::version(&s.tabs).hash(&mut h);
    rune::Tracked::version(&s.route).hash(&mut h);
    let stamp = h.finish().max(1);
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        let Some(w) = o.iter_mut().find(|w| w.id == s.window.id) else { return };
        w.reported = true;
        if !crate::servers::runtime::same_cache_refs(s, &w.pages) {
            w.pages = crate::servers::runtime::all_cache_refs(s);
        }
        if w.shown_stamp == stamp {
            return;
        }
        w.shown_stamp = stamp;
        w.shown.clear();
        for t in s.tabs.iter().filter(|t| !t.is_draft()) {
            w.shown.push((t.server.clone(), t.id.clone()));
        }
        if let crate::Route::Session { server, id } = &*s.route
            && !w.shown.iter().any(|(srv, sid)| srv == server && sid == id)
        {
            w.shown.push((server.clone(), id.clone()));
        }
    });
}

/// True when any other open window was registered but has not yet completed its first turn and
/// reported its shown tabs/route.
pub fn any_other_window_unreported(exclude_id: &str) -> bool {
    OPEN.with(|o| o.borrow().iter().any(|w| w.id != exclude_id && !w.reported))
}

/// Every session id open in a tab or active route of any OTHER open window on `server`.
pub fn other_windows_shown(exclude_id: &str, server: &opencode_kit::servers::ServerKey) -> Vec<String> {
    OPEN.with(|o| {
        o.borrow()
            .iter()
            .filter(|w| w.id != exclude_id)
            .flat_map(|w| w.shown.iter().filter(|(srv, _)| srv == server).map(|(_, sid)| sid.clone()))
            .collect()
    })
}

/// User message ids held by every OTHER open window's per-window `Pages` prefetch cache.
pub fn other_windows_cached_user_messages(exclude_id: &str) -> Vec<String> {
    OPEN.with(|o| {
        o.borrow()
            .iter()
            .filter(|w| w.id != exclude_id)
            .flat_map(|w| w.pages.iter())
            .flat_map(crate::session_pages::CacheRef::user_message_ids)
            .collect()
    })
}

/// Window `id` came to the front.
pub fn focused(id: &str) {
    let n = FOCUSES.with(|f| {
        f.set(f.get() + 1);
        f.get()
    });
    OPEN.with(|o| o.borrow_mut().iter_mut().filter(|w| w.id == id).for_each(|w| w.focused = n));
}

/// Steps window `id`'s life (`life::Life`); the list saved for the next launch follows.
/// Returns what the window does itself (keeping its frame).
pub fn life(id: &str, event: life::LifeEvent) -> life::LifeEffect {
    let effect = OPEN.with(|o| {
        let mut o = o.borrow_mut();
        let w = o.iter_mut().find(|w| w.id == id)?;
        let (next, effect) = w.life.step(event);
        w.life = next;
        o.retain(|w| w.life != life::Life::Closed);
        Some(effect)
    }).unwrap_or(life::LifeEffect::None);
    match effect {
        life::LifeEffect::SaveIds => store::save_ids(&saved_ids()),
        life::LifeEffect::Forget => {
            store::save_ids(&saved_ids());
            store::forget(id);
        }
        life::LifeEffect::None | life::LifeEffect::Keep => {}
    }
    effect
}

/// The windows that open again next launch, in the order they opened: the open ones, and one
/// kept at its close (`Life::Kept`: the last window's close button or Alt+F4, which the app's
/// quit follows).
fn saved_ids() -> Vec<String> {
    OPEN.with(|o| o.borrow().iter().filter(|w| w.life.listed()).map(|w| w.id.clone()).collect())
}

/// Window `id`'s native handle, while it's open (a kept window has closed: its frame was saved
/// as it did).
pub fn handle(id: &str) -> Option<WindowHandle> {
    OPEN.with(|o| o.borrow().iter().find(|w| w.id == id && w.life != life::Life::Kept).map(|w| w.handle.clone()))
}

/// Window `s`'s view scale (`device_scale`) before or outside `view`: its live OS window's
/// `scale_factor` (including a second monitor or after a monitor move), else the display it opened
/// on (Wayland, or before `on_created`), else `--scale` (headless).
pub fn view_scale(s: &App) -> f32 {
    let win = handle(&s.window.id)
        .and_then(|h| h.scale().map(|s| s as f32))
        .or_else(|| (s.window.scale > 0.0).then_some(s.window.scale))
        .or_else(|| crate::arg("--scale").and_then(|v| v.parse::<f32>().ok()))
        .filter(|&v| v > 0.0);
    match win {
        Some(w) => w * (*s.zoom).clamp(0.25, 4.0),
        None => session_view::scroller::device_scale(),
    }
}

/// A window's own reads and writes on `server`. The event stream is its server's, which every
/// window on it shares (`peer`).
pub fn connect(server: &opencode_kit::client::Server, wake: rune::Waker) -> (Sender<Request>, Sender<Write>, Receiver<Incoming>) {
    opencode_kit::client::connect_workers(server.clone(), wake)
}


impl App {
    /// Notes the strip's rect for drops from other windows: laid out in the view's points, kept
    /// in the window's (times the zoom), as other windows find it.
    pub(crate) fn strip_laid_out(&mut self, rect: [f32; 4]) {
        let rect = rect.map(|v| v * *self.zoom);
        self.window.strip = Some(rect);
        let id = self.window.id.clone();
        OPEN.with(|o| o.borrow_mut().iter_mut().filter(|w| w.id == id).for_each(|w| w.strip = Some(rect)));
    }

    /// `poll`: twice a second at most, reads where the window is and saves it when it moved
    /// (written behind, coalesced), so a restart, an update or a crash reopens it there too.
    pub(crate) fn poll_window(&mut self) {
        self.poll_tear();
        if self.window.strip.is_some() && self.tab_layout() == crate::layout_state::TabLayout::Inbox {
            // The inbox has no strip to drop a tab on.
            self.window.strip = None;
            let id = self.window.id.clone();
            OPEN.with(|o| o.borrow_mut().iter_mut().filter(|w| w.id == id).for_each(|w| w.strip = None));
        }
        if self.window.id.is_empty() || !self.persists() || (*self.clock - self.window.read_at.get()).abs() < 0.5 {
            return;
        }
        self.window.read_at.set(*self.clock);
        let Some(frame) = self.window_frame() else { return };
        if self.window.saved != Some(frame) {
            self.window.saved = Some(frame);
            store::save_frame(&self.window.id, frame);
        }
    }

    /// Reads where the window is (`placement`); the frame to save, unless it's minimized.
    fn window_frame(&mut self) -> Option<store::Frame> {
        let p = handle(&self.window.id)?.placement()?;
        self.set_browser_window_minimized(p.minimized);
        let (next, frame) = self.window.placement.step(p);
        if self.window.placement != next {
            self.window.placement = next;
        }
        frame
    }

    /// The window is closing. Unless it's the last one open or the app is quitting
    /// (`rune::quitting`: macOS Quit, the end of a Windows session), it's forgotten, files and
    /// all (Desktop's `registry.closed`); otherwise it opens again next launch where it was.
    pub(crate) fn window_closed(&mut self) {
        let id = self.window.id.clone();
        if id.is_empty() {
            return;
        }
        let frame = self.window_frame();
        if !self.persists() {
            OPEN.with(|o| o.borrow_mut().retain(|w| w.id != id));
            return;
        }
        let last = OPEN.with(|o| !o.borrow().iter().any(|w| w.id != id && w.life.saved()));
        // A window closed while others stay is kept, to open again (`reopen`).
        if !last && !rune::quitting() {
            // Where it was: as it closed, or (closed minimized) as it last settled.
            self.keep_closed_window(frame.or(self.window.saved));
        }
        let effect = life(&id, life::LifeEvent::Closing { keep: rune::quitting() || last });
        life(&id, life::LifeEvent::Closed);
        if effect != life::LifeEffect::Keep {
            return;
        }
        if let Some(frame) = frame {
            store::save_frame(&id, frame);
        }
    }

    /// The app starts to quit (`App::on_quit`, in every open window): the open windows' ids,
    /// this window's frame and everything it keeps go out now, so a quit, an update's restart or
    /// a Windows logoff opens every window again where it was.
    pub(crate) fn window_quitting(&mut self) {
        if self.window.id.is_empty() || !self.persists() {
            return;
        }
        store::save_ids(&saved_ids());
        if let Some(frame) = self.window_frame() {
            self.window.saved = Some(frame);
            store::save_frame(&self.window.id, frame);
        }
    }
}
