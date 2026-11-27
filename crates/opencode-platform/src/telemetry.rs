//! OpenTelemetry traces for a local Motel: OTLP/HTTP JSON to 127.0.0.1 only, posted by one
//! background thread. The UI thread only pushes finished spans onto a channel; when nothing
//! listens the batch is dropped. Attributes carry ids, counts, sizes and timings, never
//! message text or image data. `OPENCODE_RUNE_OTEL=0` turns it off; tests send only with `=1`.
//!
//! `session.open` runs from the click to the first frame that paints the session's messages.
//! Its children come from the page reader (`fetch`, `parse`, `images`), the opening gate
//! (`hold`), the frame that paints it (`first_paint`, `layout`), and after it the older page
//! (`history`) and image decodes (`images.decode`).
use std::cell::RefCell;
use std::sync::OnceLock;
use std::time::Duration;

use otlp_lite::{Ctx, Span, ms};
use rune::time::Instant;
use serde_json::Value;

#[cfg(not(target_arch = "wasm32"))]
fn to_otlp(t: Instant) -> otlp_lite::Instant { t }

#[cfg(not(target_arch = "wasm32"))]
fn from_otlp(t: otlp_lite::Instant) -> Instant { t }

#[cfg(target_arch = "wasm32")]
fn to_otlp(t: Instant) -> otlp_lite::Instant {
    let now = Instant::now();
    let otlp_now = otlp_lite::Instant::now();
    if t >= now {
        otlp_now + (t - now)
    } else {
        otlp_now.checked_sub(now - t).unwrap_or(otlp_now)
    }
}

#[cfg(target_arch = "wasm32")]
fn from_otlp(t: otlp_lite::Instant) -> Instant {
    let now = Instant::now();
    let otlp_now = otlp_lite::Instant::now();
    if t >= otlp_now {
        now + (t - otlp_now)
    } else {
        now.checked_sub(otlp_now - t).unwrap_or(now)
    }
}

fn record(parent: Ctx, name: &'static str, (start, end): (Instant, Instant), attrs: Vec<(&'static str, Value)>) {
    otlp_lite::record(parent, name, (to_otlp(start), to_otlp(end)), attrs);
}

/// After this, a late older page or decode no longer joins the open's trace.
const FOLLOW: Duration = Duration::from_secs(30);

fn on() -> bool {
    otlp_lite::start("shuvcode-rune", env!("CARGO_PKG_VERSION"), enabled().then(|| otlp_lite::endpoint("1")).flatten());
    otlp_lite::on()
}

pub fn enabled() -> bool {
    match std::env::var_os("OPENCODE_RUNE_OTEL").and_then(|v| v.into_string().ok()).as_deref() {
        Some("0") => false,
        Some("1") => true,
        _ => true,
    }
}

/// Correlate a lost reply with server admission without recording prompt text or raw URLs.
pub fn send_receipt(session: &str, id: &str, outcome: &'static str, status: u16) {
    if on() {
        Span::root("prompt.delivery").attr("session.id", session).attr("message.id", id)
            .attr("outcome", outcome).attr("http.status_code", status as i64).end_at(to_otlp(Instant::now()));
    }
}

/// One page read, measured on the reader thread.
#[derive(Clone, Debug, Default)]
pub struct Read {
    /// `disk`, or `server`.
    pub source: &'static str,
    pub fetch: Option<(Instant, Instant)>,
    /// Request to the response's first byte, and its body.
    pub first_byte: Option<Instant>,
    pub bytes: usize,
    pub json: Option<(Instant, Instant)>,
    pub parse: Option<(Instant, Instant)>,
    pub images: Option<(Instant, Instant)>,
    pub messages: usize,
    pub inline_images: usize,
    pub inline_image_bytes: usize,
    pub prepared: Vec<rune::Image>,
    pub thread: String,
}

type FrameStamp = (Instant, Option<f64>, Option<f64>);

thread_local! {
    static READ: RefCell<Option<(Instant, usize, Instant)>> = const { RefCell::new(None) };
    static OPEN: RefCell<OpenTrace> = const { RefCell::new(OpenTrace::Idle) };
    static FRAME: RefCell<Option<FrameStamp>> = const { RefCell::new(None) };
    static PAINTED: RefCell<Option<(String, usize)>> = const { RefCell::new(None) };
}

/// The live reader's first byte, body size and JSON parse start, for the page worker's `Read`.
pub fn note_read(first_byte: Instant, bytes: usize, json: Instant) {
    READ.with(|r| *r.borrow_mut() = Some((first_byte, bytes, json)));
}

pub fn take_read() -> Option<(Instant, usize, Instant)> { READ.with(|r| r.borrow_mut().take()) }

/// Lifecycle of `session.open`'s root span: open for a direct click, open for a launch-restored
/// session (which waits for a server page and may settle as `"cached"` when its disk copy already
/// painted), or settled once the root span ended (staying alive for `history.paint` or a late
/// commit span).
enum RootPhase {
    Direct { root: Span },
    Restored { root: Span, live: bool, painted: bool },
    Settled { restored: bool },
}

impl RootPhase {
    fn is_restored(&self) -> bool {
        matches!(self, Self::Restored { .. } | Self::Settled { restored: true })
    }

    fn take_root(&mut self) -> Option<Span> {
        let restored = self.is_restored();
        match std::mem::replace(self, Self::Settled { restored }) {
            Self::Direct { root } | Self::Restored { root, .. } => Some(root),
            Self::Settled { .. } => None,
        }
    }
}

struct Open {
    id: String,
    ctx: Ctx,
    started: Instant,
    phase: RootPhase,
    committed: bool,
    history: bool,
    pages: usize,
}

#[derive(Default)]
enum OpenTrace {
    #[default]
    Idle,
    Active(Box<Open>),
}

enum OpenEvent<'a> {
    BeginDirect { id: String, root: Span, ctx: Ctx, started: Instant, now: Instant },
    BeginRestored { id: String, root: Span, ctx: Ctx, started: Instant, painted: bool },
    Page { id: &'a str, history: bool, server: bool, shown_end: Instant, now: Instant },
    Committed { id: &'a str, loading: bool, now: Instant },
    Drawn { id: &'a str, rows: usize, at: Instant, now: Instant },
}

enum OpenEffect {
    EndRoot { root: Span, outcome: &'static str, rows: Option<usize>, at: Instant },
    RecordPage { ctx: Ctx, started: Instant, history: bool },
    RecordCommit { ctx: Ctx, name: &'static str, started: Instant, now: Instant, loading: bool, pages: usize },
    RecordFrame { ctx: Ctx, name: &'static str, rows: usize },
}

impl OpenTrace {
    fn expire_if_due(&mut self, now: Instant, effects: &mut Vec<OpenEffect>) {
        let expired = matches!(self, Self::Active(open) if now.saturating_duration_since(open.started) >= FOLLOW);
        if expired && let Self::Active(mut open) = std::mem::take(self) && let Some(root) = open.phase.take_root() {
            effects.push(OpenEffect::EndRoot { root, outcome: "unpainted", rows: None, at: open.started + FOLLOW });
        }
    }

    fn step(&mut self, event: OpenEvent<'_>) -> Vec<OpenEffect> {
        let mut effects = Vec::new();
        match event {
            OpenEvent::BeginDirect { id, root, ctx, started, now } => {
                if let Self::Active(mut old) = std::mem::take(self) && let Some(old_root) = old.phase.take_root() {
                    let late = now.saturating_duration_since(old.started) >= FOLLOW;
                    effects.push(OpenEffect::EndRoot {
                        root: old_root,
                        outcome: if late { "unpainted" } else { "replaced" },
                        rows: None,
                        at: if late { old.started + FOLLOW } else { now },
                    });
                }
                *self = Self::Active(Box::new(Open { id, ctx, started, phase: RootPhase::Direct { root }, committed: false, history: false, pages: 0 }));
            }
            OpenEvent::BeginRestored { id, root, ctx, started, painted } => {
                *self = Self::Active(Box::new(Open { id, ctx, started, phase: RootPhase::Restored { root, live: false, painted }, committed: false, history: false, pages: 0 }));
            }
            OpenEvent::Page { id, history, server, shown_end, now } => {
                self.expire_if_due(now, &mut effects);
                let Self::Active(open) = self else { return effects };
                if open.id != id { return effects; }
                if history {
                    open.history = true;
                } else {
                    open.pages += 1;
                    if let RootPhase::Restored { live, painted, .. } = &mut open.phase {
                        *live |= server;
                        if *live && *painted && let Some(root) = open.phase.take_root() {
                            effects.push(OpenEffect::EndRoot { root, outcome: "cached", rows: None, at: shown_end });
                        }
                    }
                }
                effects.push(OpenEffect::RecordPage { ctx: open.ctx, started: open.started, history });
            }
            OpenEvent::Committed { id, loading, now } => {
                self.expire_if_due(now, &mut effects);
                let Self::Active(open) = self else { return effects };
                if open.id != id || open.committed { return effects; }
                open.committed = true;
                let name = if open.phase.is_restored() { "launch" } else { "hold" };
                effects.push(OpenEffect::RecordCommit { ctx: open.ctx, name, started: open.started, now, loading, pages: open.pages });
            }
            OpenEvent::Drawn { id, rows, at, now } => {
                self.expire_if_due(now, &mut effects);
                let Self::Active(open) = self else { return effects };
                if open.id != id { return effects; }
                match &mut open.phase {
                    RootPhase::Direct { .. } if rows > 0 && open.committed => {
                        if let Some(root) = open.phase.take_root() {
                            effects.push(OpenEffect::RecordFrame { ctx: open.ctx, name: "first_paint", rows });
                            effects.push(OpenEffect::EndRoot { root, outcome: "painted", rows: Some(rows), at });
                        }
                    }
                    RootPhase::Restored { live, painted, .. } => {
                        if rows == 0 || !open.committed || !*live {
                            // A restored session's disk copy is up from the first frames, before its commit.
                            *painted |= rows > 0;
                        } else if let Some(root) = open.phase.take_root() {
                            effects.push(OpenEffect::RecordFrame { ctx: open.ctx, name: "first_paint", rows });
                            effects.push(OpenEffect::EndRoot { root, outcome: "painted", rows: Some(rows), at });
                        }
                    }
                    RootPhase::Settled { .. } if open.history => {
                        let ctx = open.ctx;
                        *self = Self::Idle;
                        effects.push(OpenEffect::RecordFrame { ctx, name: "history.paint", rows });
                    }
                    RootPhase::Direct { .. } | RootPhase::Settled { .. } => {}
                }
            }
        }
        effects
    }
}

fn end_root(root: Span, outcome: &'static str, rows: Option<usize>, at: Instant) {
    let root = match rows {
        Some(n) => root.attr("rows", n as i64),
        None => root,
    };
    root.attr("outcome", outcome).end_at(to_otlp(at));
}

fn step_open(event: OpenEvent<'_>) -> Vec<OpenEffect> {
    OPEN.with(|o| o.borrow_mut().step(event))
}

/// A click on session `id` (UI thread). `held`: the opening gate keeps the old page up.
pub fn open_begin(id: &str, held: bool) {
    if !on() { return; }
    let root = Span::root("session.open").attr("session.id", id).attr("held", held);
    let (ctx, started) = (root.ctx(), from_otlp(root.start));
    for effect in step_open(OpenEvent::BeginDirect { id: id.to_owned(), root, ctx, started, now: Instant::now() }) {
        if let OpenEffect::EndRoot { root, outcome, rows, at } = effect { end_root(root, outcome, rows, at); }
    }
}

static LAUNCHED: OnceLock<Instant> = OnceLock::new();
static FIRST_FRAME: OnceLock<(Instant, Option<String>, usize)> = OnceLock::new();
static PHASES: std::sync::Mutex<Vec<(&'static str, Instant, Instant)>> = std::sync::Mutex::new(Vec::new());
static LAUNCH_READS: std::sync::Mutex<Vec<(&'static str, Instant, Instant)>> = std::sync::Mutex::new(Vec::new());

pub fn startup<R>(name: &'static str, f: impl FnOnce() -> R) -> R {
    let start = Instant::now();
    let result = f();
    if FIRST_FRAME.get().is_none() { PHASES.lock().unwrap_or_else(|e| e.into_inner()).push((name, start, Instant::now())); }
    result
}

/// First thing in `main`: a restored session's open is timed from here.
pub fn launched() { LAUNCHED.get_or_init(Instant::now); }

/// One of the first server reads at launch (any thread), for the restored session's trace.
pub fn launch_read(path: &'static str, start: Instant) {
    LAUNCH_READS.lock().unwrap_or_else(|e| e.into_inner()).push((path, start, Instant::now()));
}

/// Launch reconnected to the session open at last quit; `cached` messages painted from disk.
pub fn open_restored(id: &str, cached: usize) {
    if !on() { return; }
    let start = LAUNCHED.get().copied().unwrap_or_else(Instant::now);
    let root = Span::root("session.open").at(to_otlp(start)).attr("session.id", id).attr("restored", true).attr("cached_rows", cached as i64);
    let ctx = root.ctx();
    for &(name, a, b) in PHASES.lock().unwrap_or_else(|e| e.into_inner()).iter() { record(ctx, name, (a, b), vec![]); }
    if let Some((end, shown, rows)) = FIRST_FRAME.get() {
        record(ctx, "launch.first_frame", (start, *end), vec![("restored_session", (shown.as_deref() == Some(id)).into()), ("rows", (*rows as i64).into())]);
    }
    for (path, a, b) in LAUNCH_READS.lock().unwrap_or_else(|e| e.into_inner()).drain(..) { record(ctx, "launch.read", (a, b), vec![("path", path.into())]); }
    // on_ready connects after the cached launch frame; an unchanged server page need not draw.
    let painted = PAINTED.with(|p| p.borrow().as_ref().is_some_and(|(shown, rows)| shown == id && *rows > 0));
    let _ = step_open(OpenEvent::BeginRestored { id: id.to_owned(), root, ctx, started: start, painted });
}

/// A page for `id` reached the app; `shown` is the UI thread's time merging it.
pub fn page(id: &str, history: bool, read: &Read, shown: (Instant, Instant)) {
    let effects = step_open(OpenEvent::Page { id, history, server: read.source == "server", shown_end: shown.1, now: Instant::now() });
    for effect in effects {
        match effect {
            OpenEffect::EndRoot { root, outcome, rows, at } => end_root(root, outcome, rows, at),
            OpenEffect::RecordPage { ctx, started, history } => {
                let parent = if history {
                    let span = Span::child(ctx, "history").at(to_otlp(read.fetch.map_or(shown.0, |f| f.0)))
                        .attr("messages", read.messages as i64).attr("inline_images", read.inline_images as i64)
                        .attr("inline_image_bytes", read.inline_image_bytes as i64);
                    let hctx = span.ctx();
                    span.end_at(to_otlp(shown.1));
                    hctx
                } else {
                    ctx
                };
                let n = |v: usize| Value::from(v as i64);
                let mut fetch = vec![("source", read.source.into()), ("bytes", n(read.bytes)), ("thread", read.thread.clone().into()),
                    ("before_click", read.fetch.is_some_and(|f| f.0 < started).into())];
                if let (Some((start, _)), Some(first)) = (read.fetch, read.first_byte) { fetch.push(("server_ms", ms(first - start).into())); }
                if let Some(f) = read.fetch { record(parent, "fetch", f, fetch); }
                if let Some(j) = read.json { record(parent, "json", j, vec![("bytes", n(read.bytes))]); }
                if let Some(p) = read.parse { record(parent, "parse", p, vec![("messages", n(read.messages))]); }
                if let Some(i) = read.images {
                    record(parent, "images", i, vec![("prepared", n(read.prepared.len())), ("inline_images", n(read.inline_images)),
                        ("inline_image_bytes", n(read.inline_image_bytes)), ("thread", read.thread.clone().into())]);
                }
                record(parent, "show", shown, vec![("messages", n(read.messages)), ("thread", "main".into())]);
                if !read.prepared.is_empty() {
                    let (images, since) = (read.prepared.clone(), read.images.map_or(shown.0, |i| i.0));
                    // Decodes run on Rune's image worker; this thread only waits on their handles.
                    drop(std::thread::Builder::new().name("otel-decode-wait".into()).spawn(move || {
                        let decoded = images.iter().filter(|i| i.wait().is_ok()).count();
                        record(parent, "images.decode", (since, Instant::now()), vec![("count", n(images.len())), ("decoded", n(decoded)), ("thread", "rune-images".into())]);
                    }));
                }
            }
            OpenEffect::RecordCommit { .. } | OpenEffect::RecordFrame { .. } => {}
        }
    }
}

/// The opening gate let the route change: `loading` when its hold ran out first.
pub fn committed(id: &str, loading: bool) {
    for effect in step_open(OpenEvent::Committed { id, loading, now: Instant::now() }) {
        match effect {
            OpenEffect::EndRoot { root, outcome, rows, at } => end_root(root, outcome, rows, at),
            OpenEffect::RecordCommit { ctx, name, started, now, loading, pages } => {
                record(ctx, name, (started, now), vec![("loading", loading.into()), ("pages", (pages as i64).into())]);
            }
            OpenEffect::RecordPage { .. } | OpenEffect::RecordFrame { .. } => {}
        }
    }
}

/// `on_frame_encoded`: the newest drawn frame's timing.
pub fn encoded(f: &rune::FrameEncoded) {
    if f.outcome() == rune::FrameOutcome::Drawn { FRAME.with(|x| *x.borrow_mut() = Some((f.at(), f.update_ms(), f.encode_ms()))); }
}

/// After a drawn frame: `shown` is the session on screen with its messages, if any. Ends
/// `session.open` on the first that paints the opening session, then times the older page's.
pub fn drawn(shown: Option<(&str, usize)>, (at, update_ms, encode_ms): (Instant, Option<f64>, Option<f64>)) {
    if FIRST_FRAME.get().is_none() {
        let end = Instant::now();
        drop(FIRST_FRAME.set((end, shown.map(|(id, _)| id.to_owned()), shown.map_or(0, |(_, rows)| rows))));
        if std::env::var_os("OPENCODE_RUNE_LAUNCH_REPORT").is_some() {
            for &(name, a, b) in PHASES.lock().unwrap_or_else(|e| e.into_inner()).iter() { eprintln!("{name} {:.2}", ms(b - a)); }
            let launched = LAUNCHED.get().copied().unwrap_or(end);
            eprintln!("FIRST_FRAME rows={} main_ms={:.2}", shown.map_or(0, |(_, rows)| rows), ms(end - launched));
        }
    }
    PAINTED.with(|p| {
        let mut p = p.borrow_mut();
        if p.as_ref().map(|(id, rows)| (id.as_str(), *rows)) != shown {
            *p = shown.map(|(id, rows)| (id.to_owned(), rows));
        }
    });
    let Some((id, rows)) = shown else { return };
    for effect in step_open(OpenEvent::Drawn { id, rows, at, now: Instant::now() }) {
        match effect {
            OpenEffect::EndRoot { root, outcome, rows, at } => end_root(root, outcome, rows, at),
            OpenEffect::RecordFrame { ctx, name, rows } => {
                let (update, encode) = (update_ms.unwrap_or(0.0), encode_ms.unwrap_or(0.0));
                let start = at.checked_sub(Duration::from_secs_f64((update + encode) / 1000.0)).unwrap_or(at);
                let span = Span::child(ctx, name).at(to_otlp(start)).attr("rows", rows as i64).attr("update_ms", update).attr("encode_ms", encode);
                record(span.ctx(), "layout", (start, start + Duration::from_secs_f64(update / 1000.0)), vec![("rows", (rows as i64).into())]);
                span.end_at(to_otlp(at));
            }
            OpenEffect::RecordPage { .. } | OpenEffect::RecordCommit { .. } => {}
        }
    }
}

thread_local! {
    /// By window id: the oldest streamed text it shows that no frame has drawn yet, when the
    /// hub heard it, and its session.
    static STREAMED: RefCell<std::collections::HashMap<String, (Instant, String)>> = RefCell::default();
}

/// Window `window` took a text delta of session `sid` that its view shows (`data`, a stream
/// event the hub stamped).
pub fn streamed(window: &str, sid: &str, data: &Value) {
    let Some(heard) = opencode_kit::client::heard_at(data).filter(|_| on()) else { return };
    STREAMED.with(|s| { s.borrow_mut().entry(window.to_owned()).or_insert_with(|| (heard, sid.to_owned())); });
}

/// After window `window` drew a frame at `at`: `stream.paint` runs from the hub hearing the
/// oldest delta the frame shows to the frame, so a window that draws late shows as late.
pub fn stream_drawn(window: &str, at: Instant) {
    let Some((heard, sid)) = STREAMED.with(|s| s.borrow_mut().remove(window)) else { return };
    Span::root("stream.paint").at(to_otlp(heard)).attr("window", window).attr("session.id", sid.as_str())
        .attr("latency_ms", ms(at.saturating_duration_since(heard))).end_at(to_otlp(at));
}

/// The last `encoded` frame, for `drawn`.
pub fn last_frame() -> Option<(Instant, Option<f64>, Option<f64>)> { FRAME.with(|x| x.borrow_mut().take()) }
