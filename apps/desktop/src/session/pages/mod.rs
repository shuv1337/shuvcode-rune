//! Latest-page prefetch into a bounded in-memory cache (as Desktop keeps none on disk; a restored
//! tab's first screen comes from the storage's session preview). Two read lanes keep a foreground
//! click independent of speculative reads. Neither lane writes to the server or to a file.
use crate::pickers::PickerHost as _;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc::{Receiver, Sender, channel}};
use std::time::Duration;
use rune::time::Instant;

use opencode_kit::{client::Server, model::{self, Message}};
use rune::Waker;
use serde_json::Value;

const STILL_TRYING: &str = "OpenCode isn't answering. Still trying.";
const ENTRIES: usize = 12;
const MEMORY_BYTES: usize = 64 << 20;
pub const WARM: usize = 6;
const NEAR_TOP: f32 = 200.0;

mod bridge;
mod cache_ref;
mod early;
mod machines;
mod shown;
mod worker;

pub(crate) use cache_ref::CacheRef;
pub use early::Early;
pub use machines::{History, Key, LATEST, OpenEvent, Opening};
use machines::{HOLD, Histories, HistoryEffect, HistoryEvent, Stall};

/// Session-page prefetch pool, each session's history above its latest page, and older pages
/// on scroll.
#[derive(Default)]
pub struct State {
    pool: Option<Pages>,
    /// Each session's history, by (server, session) (`machines::History`).
    histories: Histories,
    /// The session whose timeline stands near its top, told by its scroll viewport.
    near_top: std::rc::Rc<std::cell::RefCell<String>>,
    /// Shared waker slot so `watch_top` closures built before `State::set` still wake the window.
    wake: std::rc::Rc<std::cell::RefCell<Option<Waker>>>,
    /// The calm "still trying" toast (`machines::Stall`).
    stall: Stall,
    stall_after: Option<Duration>,
    /// Sessions whose boot preview was an island above the latest page (its place's turn, not
    /// the way to the end; `restore::preview`), by the island's last message: history fills
    /// the gap under it before it reads above it.
    gaps: std::collections::HashMap<String, String>,
    /// The newest pages read again that do not reach what is loaded yet (`bridge`).
    bridges: HashMap<String, bridge::Held>,
}

/// How long the server may stay silent before the app says so.
const STALL_AFTER: Duration = Duration::from_secs(30);

impl State {
    pub fn set(&mut self, pages: Pages) {
        *self.wake.borrow_mut() = pages.wake.clone();
        self.histories.clear();
        self.pool = Some(pages);
    }

    /// `App::rest`: keeps only pages, histories, gaps, and bridges for sessions `keep` names.
    pub fn rest(&mut self, keep: &dyn Fn(&str) -> bool) {
        let server = self.server().to_owned();
        if let Some(pool) = &self.pool && let QueueState::Running(q) = &mut *pool.shared.0.lock().unwrap_or_else(PoisonError::into_inner) {
            q.ready.retain(|p| keep(&p.key.id));
        }
        self.histories.retain(&server, keep);
        self.gaps.retain(|id, _| keep(id));
        self.bridges.retain(|id, _| keep(id));
    }

    /// Drops one closed session's prefetched page, history, gap, and bridge immediately.
    pub fn forget(&mut self, id: &str) {
        let server = self.server().to_owned();
        if let Some(pool) = &self.pool && let QueueState::Running(q) = &mut *pool.shared.0.lock().unwrap_or_else(PoisonError::into_inner) {
            q.ready.retain(|p| p.key.id != id);
        }
        self.histories.forget(&server, id);
        self.gaps.remove(id);
        self.bridges.remove(id);
    }

    /// The user messages of the pages in the reader's cache, which a tab shows when it opens.
    pub fn user_message_ids(&self) -> Vec<String> {
        let Some(pool) = &self.pool else { return Vec::new() };
        let guard = pool.shared.0.lock().unwrap_or_else(PoisonError::into_inner);
        let QueueState::Running(q) = &*guard else { return Vec::new() };
        q.ready.iter().flat_map(|p| p.messages.iter()).filter_map(|m| match m { Message::User { id, .. } => Some(id.clone()), _ => None }).collect()
    }

    pub fn has_pool(&self) -> bool {
        self.pool.is_some()
    }

    /// The server the pool reads (its base URL; empty for a fixture's).
    pub(crate) fn server(&self) -> &str { self.pool.as_ref().map_or("", |p| p.server.as_str()) }

    /// For session `id`'s timeline scroll viewport: standing near its top, the next poll reads
    /// the page before it.
    pub fn watch_top(&self, id: &str) -> impl Fn(rune::ScrollPos) + 'static {
        let (near, wake, id) = (self.near_top.clone(), self.wake.clone(), id.to_owned());
        move |at| {
            let is = at.offset[1] < NEAR_TOP;
            let mut n = near.borrow_mut();
            if is && *n != id {
                *n = id.clone();
                drop(n);
                // The scroll may have stopped on this frame; poll without waiting for another.
                let wake = wake.borrow().clone();
                if let Some(wake) = wake { wake(); }
            } else if !is && *n == id { n.clear(); }
        }
    }

    /// Whether `id`'s transcript has a gap under its boot preview still to read.
    pub fn gap(&self, id: &str) -> bool {
        self.gaps.contains_key(id)
    }

    /// Whether `id`'s first read is done: its latest page and the history behind it landed.
    pub fn first_read_done(&self, id: &str) -> bool {
        matches!(self.histories.get(self.server(), id), Some(History::Idle { .. } | History::Loading { .. } | History::Failed { .. } | History::Exhausted))
    }

    /// The session whose timeline stands near its top; empty when none does.
    pub fn near_top(&self) -> std::cell::Ref<'_, String> {
        self.near_top.borrow()
    }

    /// An older page for `id` has taken longer than the opening hold: show that it's coming.
    pub fn loading_older(&self, id: &str) -> bool {
        matches!(self.histories.get(self.server(), id), Some(History::Loading { slow: true, .. }))
    }

    /// Whether a page before `id`'s oldest loaded message is still to read.
    pub fn more_above(&self, id: &str) -> bool {
        matches!(self.histories.get(self.server(), id), Some(History::Idle { .. } | History::Loading { .. } | History::Failed { .. }))
    }

    /// Whether session `id`'s history state is awaiting a `Before(cursor)` page.
    pub(super) fn awaits_history(&self, id: &str, cursor: &str) -> bool {
        self.histories.awaits(self.server(), id, cursor)
    }

}

#[derive(Clone)]
pub struct Page {
    pub key: Key,
    pub messages: Vec<Message>,
    pub cursor: Option<String>,
    /// Its messages' ids, kinds and times, in the server's order (context_window).
    pub stubs: Vec<crate::context_window::Stub>,
    /// How many messages the read asked for: a page with fewer is the session's first.
    pub asked: usize,
    pub history: bool,
    /// Which read it answers (`history` for every one but the newest page's).
    pub read_of: bridge::ReadOf,
    pub images: Vec<crate::attachments::PreparedImage>,
    pub(crate) bytes: usize,
    /// How the reader got it, for the open's trace.
    pub read: crate::telemetry::Read,
}

#[derive(Clone, PartialEq, Eq)]
struct Job { key: Key, cursor: Option<String>, limit: usize, foreground: bool, one: bool }


/// Whether the open lane's reads are getting answers, or since when they began failing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LaneHealth { #[default] Healthy, Stalled { since: Instant } }
enum LaneHealthEvent { TransientFailure(Instant), Answered, Forgiven }

impl LaneHealth {
    fn step(&mut self, event: LaneHealthEvent) {
        match (*self, event) {
            (Self::Healthy, LaneHealthEvent::TransientFailure(since)) => *self = Self::Stalled { since },
            (Self::Stalled { .. }, LaneHealthEvent::TransientFailure(_)) => {}
            (_, LaneHealthEvent::Answered | LaneHealthEvent::Forgiven) => *self = Self::Healthy,
        }
    }

    fn stalled_for(self, now: Instant) -> Option<Duration> {
        match self { Self::Healthy => None, Self::Stalled { since } => Some(now.saturating_duration_since(since)) }
    }
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    active: HashSet<(Key, Option<String>)>,
    ready: VecDeque<Arc<Page>>,
    failed: HashMap<Key, Instant>,
    /// When the open lane's reads began failing to get an answer; cleared by one that does.
    health: LaneHealth,
}

/// Lifecycle of the shared reader queue: running until [`Pages`] drops.
enum QueueState { Running(Box<Queue>), Stopped }
enum QueueEvent { Stop, Health(LaneHealthEvent) }

impl Default for QueueState { fn default() -> Self { Self::Running(Box::default()) } }

impl QueueState {
    fn is_running(&self) -> bool { matches!(self, Self::Running(_)) }
    fn step(&mut self, event: QueueEvent) -> Option<Box<Queue>> {
        match (self, event) {
            (state @ Self::Running(_), QueueEvent::Stop) => match std::mem::replace(state, Self::Stopped) {
                Self::Running(old) => Some(old),
                Self::Stopped => None,
            },
            (Self::Running(q), QueueEvent::Health(ev)) => { q.health.step(ev); None }
            (Self::Stopped, _) => None,
        }
    }
}

type Shared = Arc<(Mutex<QueueState>, Condvar)>;
pub type Fetch = Arc<dyn Fn(&str) -> Result<Value, opencode_kit::wire_error::WireError> + Send + Sync>;
pub type PageResult = Result<Arc<Page>, (Key, bridge::ReadOf, opencode_kit::wire_error::WireError)>;

pub struct Pages {
    shared: Shared,
    pub results: Receiver<PageResult>,
    wake: Option<Waker>,
    /// The server it reads (its base URL; empty for a fixture's or a test's).
    server: String,
    /// What the web's lanes read with: a page has no threads to wait on the queue, so each
    /// request starts a task that reads what's queued.
    #[cfg(target_arch = "wasm32")]
    lanes: Option<(Fetch, Sender<PageResult>, Waker, Arc<rune::ImageCache>)>,
}

impl Drop for Pages {
    fn drop(&mut self) {
        let old = { let mut g = self.shared.0.lock().unwrap_or_else(PoisonError::into_inner); let old = g.step(QueueEvent::Stop); self.shared.1.notify_all(); old };
        drop(old);
    }
}

impl Pages {
    /// In-process delivery for deterministic stories/tests; no server or worker is started.
    pub(crate) fn fixture(ready: Option<Arc<Page>>) -> (Self, Sender<PageResult>) {
        let (send, results) = channel();
        let mut q = Queue::default();
        q.ready.extend(ready);
        (Self { shared: Arc::new((Mutex::new(QueueState::Running(Box::new(q))), Condvar::new())), results, wake: None, server: String::new(), #[cfg(target_arch = "wasm32")] lanes: None }, send)
    }
    pub fn live(server: Server, wake: Waker, early: Option<Early>, images: Arc<rune::ImageCache>) -> Self {
        let base = server.base.clone();
        let mut pages = Self::new(early::fetch(server, early), wake, images);
        pages.server = base;
        pages
    }

    pub fn new(fetch: Fetch, wake: Waker, images: Arc<rune::ImageCache>) -> Self {
        let shared = Arc::new((Mutex::new(QueueState::default()), Condvar::new()));
        let (out, results) = channel();
        #[cfg(target_arch = "wasm32")]
        return Self { shared, results, wake: Some(wake.clone()), server: String::new(), lanes: Some((fetch, out, wake, images)) };
        #[cfg(not(target_arch = "wasm32"))]
        {
            for foreground in [true, false] {
                let (q, fetch, out, wake, images) = (shared.clone(), fetch.clone(), out.clone(), wake.clone(), images.clone());
                let name = format!("session-{}", if foreground { "open" } else { "prefetch" });
                if let Err(e) = std::thread::Builder::new().name(name.clone()).spawn(move || worker::worker(q, fetch, out, wake, images, foreground)) {
                    eprintln!("opencode-rune: couldn't spawn {name} thread: {e}");
                }
            }
            Self { shared, results, wake: Some(wake), server: String::new() }
        }
    }

    /// How long the open lane has gone without an answer from the server, if it has.
    pub fn stalled_for(&self) -> Option<Duration> {
        match &*self.shared.0.lock().unwrap_or_else(PoisonError::into_inner) {
            QueueState::Running(q) => q.health.stalled_for(now()),
            QueueState::Stopped => None,
        }
    }

    /// Starts the silence over, as when the event stream comes back.
    pub fn forgive(&self) {
        self.shared.0.lock().unwrap_or_else(PoisonError::into_inner).step(QueueEvent::Health(LaneHealthEvent::Forgiven));
    }

    /// Memory is ready before the worker wakes the app, so a click racing that wake can paint.
    pub fn peek(&self, id: &str) -> Option<Arc<Page>> {
        let mut guard = self.shared.0.lock().unwrap_or_else(PoisonError::into_inner);
        let QueueState::Running(q) = &mut *guard else { return None };
        let at = q.ready.iter().position(|p| p.key.id == id)?;
        let page = q.ready.remove(at)?;
        q.ready.push_back(page.clone());
        Some(page)
    }

    pub fn request(&self, key: Key, foreground: bool) {
        self.enqueue(Job { key, foreground, cursor: None, limit: LATEST, one: false });
    }

    /// The `limit` messages before `cursor`.
    pub fn history(&self, key: Key, cursor: String, limit: usize) {
        self.enqueue(Job { key, foreground: false, cursor: Some(cursor), limit, one: false });
    }

    fn enqueue(&self, job: Job) {
        let mut guard = self.shared.0.lock().unwrap_or_else(PoisonError::into_inner);
        let QueueState::Running(q) = &mut *guard else { return };
        let speculative = !job.foreground && job.cursor.is_none();
        q.failed.retain(|_, until| *until > Instant::now());
        if speculative && q.failed.contains_key(&job.key) { return; }
        if speculative && q.ready.iter().any(|p| p.key == job.key) { return; }
        if q.active.contains(&(job.key.clone(), job.cursor.clone())) { return; }
        if let Some(old) = q.jobs.iter_mut().find(|j| j.key.id == job.key.id && j.cursor == job.cursor && j.one == job.one) {
            old.foreground |= job.foreground;
            old.limit = old.limit.max(job.limit);
            if job.key.updated > old.key.updated { old.key = job.key; }
        } else {
            // A pointer sweeping the list cannot leave an unbounded speculative backlog, and must
            // never evict an explicit history/bridge read.
            if speculative
                && q.jobs.iter().filter(|j| !j.foreground && j.cursor.is_none()).count() >= ENTRIES
                && let Some(i) = q.jobs.iter().position(|j| !j.foreground && j.cursor.is_none())
            {
                q.jobs.remove(i);
            }
            q.jobs.push_back(job);
        }
        self.shared.1.notify_all();
        drop(guard);
        #[cfg(target_arch = "wasm32")]
        if let Some((fetch, out, wake, images)) = self.lanes.clone() {
            let shared = self.shared.clone();
            rune::task::spawn("session-pages", move || {
                for foreground in [true, false] {
                    while let Some(job) = worker::take_now(&shared, foreground) {
                        if !worker::read(&shared, &fetch, &out, &wake, &images, foreground, job) { return; }
                    }
                }
            })
            .ok();
        }
    }
}

fn now() -> Instant { Instant::now() }


impl crate::App {
    pub fn cancel_open(&mut self, tx: &mut rune::Tx) {
        if let Some(opening) = self.opening.take() {
            *self.route = opening.from;
            let _ = self.show_draft();
            session_view::jump::settle(self, tx);
        }
    }

    pub fn begin_open(&mut self, id: &str, tx: &mut rune::Tx) {
        self.close_picker(false);
        crate::subagents::shells::set_open(self, false);
        *self.kernel.slots.get_mut(&crate::todos::TODO_OPEN) = false;
        let from = self.opening.as_ref().filter(|o| !o.loading()).map_or_else(|| (*self.route).clone(), |o| o.from.clone());
        *self.opening = Some(Opening::new(id, from, self.transcripts.contains_key(id), *self.clock));
        self.open_now(id, tx);
        self.open_page(id, tx);
        self.poll_open(tx);
        if self.opening.is_some() { tx.wake_after(HOLD); }
    }

    fn poll_open(&mut self, tx: &mut rune::Tx) -> bool {
        let Some(opening) = &*self.opening else { return false };
        if self.route != opening.from && self.route != crate::Route::here(opening.id.clone()) {
            *self.opening = None;
            return false;
        }
        let id = opening.id.clone();
        let ready = opening.received() && self.transcripts.get(&id).is_some_and(|t| self.composer_ext.attach.transcript_ready(t));
        if ready {
            *self.opening = None;
            // Mount the finished transcript at its end, or where it was read, without
            // resetting input or focus.
            session_view::jump::settle(self, tx);
            self.land_place(&id, tx);
            if let Some(page) = self.pages.pool.as_ref().and_then(|p| p.peek(&id)) { self.schedule_history(&page, tx); }
            return true;
        }
        let expired = opening.expired(self.live(), *self.clock);
        if expired && self.opening.as_mut().is_some_and(|o| o.step(OpenEvent::Expired)) {
            session_view::jump::settle(self, tx);
            return true;
        }
        false
    }

    /// The session on screen with its messages painted (not held, not loading), for traces.
    pub fn painted_session(&self) -> Option<(&str, usize)> {
        let crate::Route::Session { id, .. } = &*self.route else { return None };
        if self.opening.is_some() { return None; }
        Some((id, self.transcripts.get(id).map_or(0, |t| t.messages.len())))
    }

    pub fn loading_session(&self, id: &str) -> bool {
        self.opening.as_ref().is_some_and(|o| o.id == id && o.loading())
    }

    /// The held transcript is read-only; the route and composer already belong to the target.
    pub(crate) fn displayed_session<'a>(&'a self, id: &'a str) -> Option<&'a str> {
        match self.opening.as_ref().filter(|o| o.id == id && !o.loading()) {
            Some(o) => match &o.from { crate::Route::Session { id: from, .. } => Some(from), _ => None },
            None => Some(id),
        }
    }

    pub(crate) fn page_key(&self, id: &str) -> Key {
        let updated = self.session(id).or_else(|| self.kernel.slots.get(&crate::switcher::SWITCHER).loaded.iter().find(|s| s.id == id))
            .map_or(0, |s| s.updated.max(s.busy.unwrap_or(0)).max(s.idle.unwrap_or(0)));
        Key { id: id.to_owned(), updated }
    }

    pub(crate) fn reload_session(&self, id: &str) {
        let crate::Source::Live { requests: Some(r), .. } = &*self.source else { return };
        match &self.pages.pool {
            Some(pages) => pages.request(self.page_key(id), true),
            None => {
                r.send(opencode_kit::client::Request::Session(id.to_owned())).ok();
            }
        }
    }

    pub fn prefetch_session(&self, key: &str) {
        // A draft tab has no session to read yet, and another server's tab reads as it opens.
        let id = match self.tab(key) {
            Some(t) if t.is_draft() || t.server != *crate::servers::runtime::active(self) => return,
            Some(t) => t.id.as_str(),
            None => key,
        };
        // A session near the pointer or the keyboard: what its screen draws from plugins starts ahead.
        crate::plugin_host::dormant::near_session(self, id);
        self.prefetch_page(id);
    }

    /// Reads session `id`'s newest page on the speculative lane, as launch warms the newest
    /// sessions: no one is near it, so nothing else starts for it.
    pub fn prefetch_page(&self, id: &str) {
        if let Some(pages) = &self.pages.pool { pages.request(self.page_key(id), false); }
    }

    /// Runs before Rune's list focus traversal; start the next highlighted row's read now.
    pub fn prefetch_navigation(&self, ids: &[String], index: usize, ev: &rune::KeyEvent) -> bool {
        use rune::Key;
        if ids.is_empty() { return false; }
        let next = match ev.key {
            Key::Down | Key::Tab if !ev.mods.shift => (index + 1).min(ids.len() - 1),
            Key::Up | Key::Tab => index.saturating_sub(1),
            Key::Home => 0,
            Key::End => ids.len() - 1,
            _ => index,
        };
        self.prefetch_session(&ids[next]);
        false
    }

    pub fn open_page(&mut self, id: &str, tx: &mut rune::Tx) {
        let Some(pages) = &self.pages.pool else { return };
        let ready = pages.peek(id);
        pages.request(self.page_key(id), true);
        if let Some(page) = ready {
            if !self.transcripts.contains_key(id) { self.show_page(&page, tx); }
            else { self.schedule_history(&page, tx); }
        }
    }

    pub(crate) fn show_page(&mut self, page: &Page, tx: &mut rune::Tx) {
        self.tail_page(&page.key.id);
        let shown = Instant::now();
        self.merge_page(page, tx);
        crate::telemetry::page(&page.key.id, page.history, &page.read, (shown, Instant::now()));
    }

    pub(super) fn merge_joined(&mut self, page: &Page, tx: &mut rune::Tx) {
        // A page this window read goes into the server's copy, for its other windows too.
        crate::windows::peer::touch(self);
        if let Some(o) = &mut *self.opening && o.id == page.key.id && !page.history { o.step(OpenEvent::Received); }
        self.composer_ext.attach.adopt(&page.images);
        let id = &page.key.id;
        let stale = self.page_key(id).updated > page.key.updated;
        let added = page.messages.iter().filter(|m| !self.transcripts.get(id).is_some_and(|t| t.messages.iter().any(|old| old.id() == m.id()))).count();
        let first = !self.transcripts.contains_key(id);
        let t = self.transcripts.entry(id.clone()).or_insert_with(|| model::Transcript { session: id.clone(), messages: vec![] });
        let mut messages = page.messages.clone();
        // Memory and older history must never rewind a live delta or local echo.
        if page.history || stale { messages.retain(|m| !t.messages.iter().any(|old| old.id() == m.id())); }
        crate::attachments::reconcile(&mut self.composer_ext.attach.echoes, t, &messages);
        let delivered: Vec<_> = messages.iter().map(|m| m.id().to_owned()).collect();
        // The latest page landing on a boot preview it doesn't reach: the preview was the
        // place's turn (`restore::preview`), and the messages between it and the page are still
        // to read. They come first, newest page first, each under the preview.
        let cold = self.launch.cold_transcript.as_ref().is_some_and(|c| c.id == *id);
        if cold && !page.history && added == page.messages.len()
            && let (Some(last), Some(next)) = (t.messages.last(), page.messages.first())
            && next.id() > last.id()
        {
            self.pages.gaps.insert(id.clone(), last.id().to_owned());
        }
        if page.history {
            match self.pages.gaps.get(id.as_str()).cloned() {
                Some(island) => {
                    // Under the island what's newer than its last message, in the page's order;
                    // above it, as below, what's older. A page reaching the island closes the gap.
                    let under = t.messages.iter().position(|m| m.id() == island).map_or(0, |i| i + 1);
                    let closed = page.cursor.is_none() || page.messages.iter().any(|m| m.id() <= island.as_str());
                    let (newer, mut older): (Vec<_>, Vec<_>) = messages.into_iter().partition(|m| m.id() > island.as_str());
                    t.messages.splice(under..under, newer);
                    older.append(&mut t.messages);
                    t.messages = older;
                    if closed {
                        self.pages.gaps.remove(id.as_str());
                    }
                }
                None => {
                    // Cursor order, not ID order: a steered prompt can have an older ID than history.
                    messages.append(&mut t.messages);
                    t.messages = messages;
                }
            }
        } else {
            match self.kernel.slots.get_mut(&crate::events::STREAM_GAPS).0.get_mut(id.as_str()) {
                Some(gapped) => t.merge_caught(messages, gapped),
                None => t.merge(messages),
            }
        }
        self.settle_inbox(id, &delivered);
        if cold { self.launch.cold_transcript = None; }
        // A timeline read up from its end (a place kept from the last run) isn't carried to it.
        if first || cold { session_view::jump::settle(self, tx); }
        // A place coming back whose row this page brought lands in this commit
        // (`session_view::returning`); one still further up waits on for the next.
        if !self.land_place(id, tx) {
            self.timeline_fx.ends.progress(id);
        }
        if page.history {
            // A page whose oldest raw server message didn't move strictly earlier (or repeated its
            // own cursor) stops paging instead of looping (`history-machine.ts` `stalled`). Tracking
            // raw server `stubs` keeps paging across pages of non-rendered message types (`synthetic`,
            // `system`, `idle`) and when history re-enters on an already-populated shared transcript.
            let id = id.clone();
            if let bridge::ReadOf::Before(sent) = &page.read_of {
                let oldest = crate::context_window::oldest_position(&page.stubs);
                self.step_history(&id, HistoryEvent::Landed { sent, cursor: page.cursor.clone(), oldest, added }, tx);
            }
            // Where the page leaves the reader is reported next frame; read on only from there.
            let mut near = self.pages.near_top.borrow_mut();
            if *near == *id { near.clear(); }
            drop(near);
            self.context_window_landed(&id);
        }
        self.window_read(&page.key.id, &page.stubs, page.history, page.cursor.is_none() || page.stubs.len() < page.asked);
        self.schedule_history(page, tx);
    }

    fn schedule_history(&mut self, page: &Page, tx: &mut rune::Tx) {
        let id = &page.key.id;
        if self.route == crate::Route::here(id.clone()) && !page.history && let Some(cursor) = &page.cursor {
            // Give the latest page its first frame before expanding history.
            let id = id.clone();
            let oldest = crate::context_window::oldest_position(&page.stubs);
            self.step_history(&id, HistoryEvent::Latest { key: page.key.clone(), cursor, oldest, now: Instant::now() }, tx);
        }
    }

    /// Steps `id`'s history and carries out what it asks for. True when what shows changed.
    fn step_history(&mut self, id: &str, event: HistoryEvent<'_>, tx: &mut rune::Tx) -> bool {
        let server = self.pages.server().to_owned();
        let effects = self.pages.histories.step(&server, id, event);
        let mut changed = false;
        for effect in effects {
            match effect {
                HistoryEffect::Read { key, cursor, limit } => {
                    let key = key.unwrap_or_else(|| self.page_key(id));
                    if let Some(pages) = &self.pages.pool { pages.history(key, cursor, limit); }
                }
                HistoryEffect::WakeAfter(after) => tx.wake_after(after),
                HistoryEffect::Settle => {
                    if self.route == crate::Route::here(id.to_owned()) {
                        session_view::jump::settle(self, tx);
                    }
                }
                HistoryEffect::Repaint => changed = true,
            }
        }
        changed
    }

    pub fn poll_pages(&mut self, tx: &mut rune::Tx) -> bool {
        let results: Vec<_> = self.pages.pool.as_ref().map(|p| p.results.try_iter().collect()).unwrap_or_default();
        let mut changed = self.read_shown_session(tx);
        for result in results {
            if let Ok(page) = &result {
                // A prefetched page for a highlighted result warms the page cache, not the
                // current view. Redraw only for a receipt it settled, or a page shown below.
                changed |= self.judge_sends_by_page(&page.key.id, &page.messages);
                self.check_sends();
            }
            match result {
                Ok(page) if self.route == crate::Route::here(page.key.id.clone()) || self.opening.as_ref().is_some_and(|o| o.id == page.key.id) || (page.history && self.transcripts.contains_key(&page.key.id)) => { self.show_page(&page, tx); changed = true; }
                Err((key, bridge::ReadOf::Newest, error)) if self.opening.as_ref().is_some_and(|o| o.id == key.id) && error.status() == Some(404) => {
                    *self.opening = None;
                    changed |= crate::session_state::missing(self, &key.id, tx);
                }
                Err((key, bridge::ReadOf::Newest, error)) if self.opening.as_ref().is_some_and(|o| o.id == key.id) => {
                    *self.diag.status = error.to_string();
                    // What the timeline's loading row says: why, from the typed error (`crate::errors`).
                    let server = crate::servers::runtime::label(crate::servers::runtime::active(self));
                    let why = crate::errors::UserError::from_wire("Couldn't load this session", &server, &error).description().replace('\n', " ");
                    if let Some(o) = &mut *self.opening {
                        o.step(match error.kind {
                            opencode_kit::wire_error::WireKind::PartialOutput {head,..} => OpenEvent::Partial(why,head),
                            _=>OpenEvent::Failed(why),
                        });
                    }
                    changed = true;
                }
                Err((key, bridge::ReadOf::Newest, error)) if self.route == crate::Route::here(key.id.clone()) && error.status() == Some(404) => {
                    changed |= crate::session_state::missing(self, &key.id, tx);
                }
                Err((key, bridge::ReadOf::Newest, error)) if self.route == crate::Route::here(key.id.clone()) => {
                    self.pages.bridges.remove(&key.id);
                    // A busy server is waited out; only silence that lasts is told (below).
                    if error.transient() { eprintln!("opencode-rune: session read failed: {error}"); continue; }
                    crate::status_toast::failed_wire(self, "Couldn't load this session", &error);
                    changed = true;
                }
                Err((key, bridge::ReadOf::Before(cursor), error)) => {
                    // Always settle an in-flight bridge or history read even after switching tabs,
                    // so `History::Filling` / `Loading` never hang until timeout.
                    if self.pages.bridges.get(&key.id).is_some_and(|h| h.awaits(&cursor)) {
                        self.pages.bridges.remove(&key.id);
                    }
                    changed |= self.step_history(&key.id, HistoryEvent::Failed { sent: &cursor, now: Instant::now() }, tx);
                    if self.route == crate::Route::here(key.id.clone()) {
                        if error.transient() { eprintln!("opencode-rune: session read failed: {error}"); continue; }
                        crate::status_toast::failed_wire(self, "Couldn't load this session", &error);
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        changed |= self.poll_open(tx);
        changed |= self.tell_stall();
        // Each timed history (the first page's wait, a slow or stuck older page).
        let timed = self.pages.histories.timed(self.pages.server());
        let server = if timed.is_empty() { String::new() } else { self.pages.server().to_owned() };
        for id in timed {
            let routed = self.route == crate::Route::here(id.clone());
            let expanding = matches!(self.pages.histories.get(&server, &id), Some(History::Expanding { .. }));
            if routed || !expanding { changed |= self.step_history(&id, HistoryEvent::Tick { now: Instant::now() }, tx); }
        }
        changed | self.load_older(tx)
    }

    /// Says once, calmly, that the server has stayed silent, and takes it back when it answers.
    fn tell_stall(&mut self) -> bool {
        // A connection drop or an unsent prompt already has an inline status. A slow page
        // read from the same outage must not also cover the composer with a waiting toast.
        let silent = if self.unsent.waiting_for_link() || crate::reconnect::down(self) { None } else { self.pages.pool.as_ref().and_then(Pages::stalled_for) };
        let after = self.pages.stall_after.unwrap_or(STALL_AFTER);
        match (silent, self.pages.stall) {
            (Some(for_how_long), Stall::Quiet) if for_how_long >= after => {
                self.pages.stall = Stall::Told { toast: crate::status_toast::waiting(self, STILL_TRYING) };
                true
            }
            (None, Stall::Told { toast }) => {
                self.pages.stall = Stall::Quiet;
                crate::status_toast::dismiss_id(self, toast);
                true
            }
            _ => false,
        }
    }

    /// The event stream is back: the silence is over, whatever the open session's read says.
    pub(crate) fn stream_back(&mut self) {
        if let Some(pool) = &self.pages.pool { pool.forgive(); }
        self.tell_stall();
    }

    /// A place coming back whose row is older than what's loaded (`session_view::returning`)
    /// reads on up to it, a page at a time, as a reader scrolling up would.
    pub(crate) fn read_toward_place(&mut self, id: &str, tx: &mut rune::Tx) {
        if self.pages.pool.is_some() {
            self.step_history(id, HistoryEvent::NearTop { now: Instant::now() }, tx);
        }
    }

    /// Reads the page before the open session's oldest message once its timeline nears the
    /// top. It lands above what's on screen without moving it: the virtual list holds its
    /// first visible row (Rune `VirtualList::anchor_delta`).
    /// Reads the page before `id`'s oldest loaded message, as the reader's scroll to the top
    /// would (Find opened on a message not loaded yet). Nothing while one is on its way.
    pub(crate) fn read_older(&mut self, id: &str, tx: &mut rune::Tx) -> bool {
        if self.pages.pool.is_none() { return false; }
        self.step_history(id, HistoryEvent::NearTop { now: Instant::now() }, tx)
    }

    fn load_older(&mut self, tx: &mut rune::Tx) -> bool {
        let crate::Route::Session { id, .. } = &*self.route else { return false };
        if *self.pages.near_top.borrow() != *id || self.pages.pool.is_none() { return false; }
        let id = id.clone();
        self.step_history(&id, HistoryEvent::NearTop { now: Instant::now() }, tx)
    }
}
