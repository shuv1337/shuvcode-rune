//! Pictures for markdown in replies, as Desktop reads them: an image's file through the server
//! (`readLocalImage`: a relative path in the session's directory, an absolute one in its own),
//! a web image and an external link's favicon (`markdown.tsx`'s `markExternalLinkFavicons`)
//! from the web. Each is fetched once on a worker thread and decoded by Rune's image cache.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rune::{Image, ImageCache, Pixels, Waker};

use super::cache::{FAVICON_MAX_PT, MD_MAX_PT, dimensions, display_scale, live_weight, load_thumb, md_effective_pt, scaled_weight};

/// Maximum decoded CPU pixel bytes kept in `MdStore`.
pub(super) const MD_PIXEL_BUDGET: usize = 32 << 20;
/// Tighter decoded pixel budget kept when resting (leaves room for active viewport markdown images).
pub(super) const MD_REST_PIXEL_BUDGET: usize = 24 << 20;
/// Maximum encoded bytes kept in `MdStore` across `Decoding` and `Held` slots.
pub(super) const MD_BYTES_BUDGET: usize = 24 << 20;
/// Maximum simultaneous markdown image fetch+decode worker tasks (bounds peak RAM).
const MAX_READERS: usize = 4;

/// Reads a file through the server: `(directory, path)` to its bytes.
pub type FileFetch = Arc<dyn Fn(&str, &str) -> Result<Vec<u8>, String> + Send + Sync>;
/// Fetches a URL from the web.
pub type WebFetch = Arc<dyn Fn(&str) -> Result<Vec<u8>, String> + Send + Sync>;

/// What a source is fetched from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Source {
    File { directory: String, path: String },
    Web(String),
}

fn source_mime(source: &Source) -> &'static str {
    let name = match source {
        Source::File { path, .. } => path.as_str(),
        Source::Web(url) => url.split(['?', '#']).next().unwrap_or(""),
    };
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("apng") => "image/apng",
        Some("svg") => "image/svg+xml",
        Some("jpg" | "jpeg") => "image/jpeg",
        _ => "image/png",
    }
}

/// Where a source's picture is.
enum Slot {
    Fetching,
    /// The decoding/decoded scaled picture and its encoded bytes (kept intact so lightbox/copy/drag
    /// always work; current-frame entries are protected from eviction).
    Decoding { image: Image, bytes: Arc<[u8]>, size: Option<[u32; 2]>, weight: usize, last_used: u64, last_frame: u64 },
    /// Its encoded bytes only: let go of its decoded CPU pixels (`rest` / LRU).
    /// The next ask decodes them again, with no fetch.
    Held { bytes: Arc<[u8]>, size: Option<[u32; 2]>, last_used: u64 },
    Failed(String),
}

/// Where a site's favicon stands (`site-icon.tsx`'s `loading | loaded | failed`).
pub enum SiteIcon {
    Loading,
    Loaded(Pixels),
    Failed,
}

/// Where pictures are fetched from: the live server's file reads and the web.
#[derive(Clone)]
struct Fetches {
    files: FileFetch,
    web: WebFetch,
}

/// Whether markdown can fetch pictures yet. A window draws a restored session from its saved
/// copy before it connects, so images wait for the host's waker and fetches; the worker starts
/// with the first image asked for, and a new fetch (a reconnect, another server) or waker
/// starts a new one, the old one finishing what it was asked first.
type ReadWork = (Source, f32, f32);
type ReadPool = Arc<Mutex<(VecDeque<ReadWork>, usize)>>;

enum Enqueue {
    /// Fetcher has no waker or fetch callback yet; leave slot absent so it retries once wired.
    Unready,
    Queued,
    Failed,
}

enum Fetcher {
    Unset { waker: Option<Waker>, fetch: Option<Fetches> },
    Ready { waker: Waker, fetch: Fetches },
    Running { waker: Waker, fetch: Fetches, pool: ReadPool },
}

impl Default for Fetcher {
    fn default() -> Self {
        Fetcher::Unset { waker: None, fetch: None }
    }
}

enum FetcherEvent {
    Waker(Waker),
    Fetch(Fetches),
    /// An image asked for a fetch.
    Start(ReadPool),
}

fn step(state: Fetcher, event: FetcherEvent) -> Fetcher {
    use Fetcher::{Ready, Running, Unset};
    let ready = |waker: Option<Waker>, fetch: Option<Fetches>| match (waker, fetch) {
        (Some(waker), Some(fetch)) => Ready { waker, fetch },
        (waker, fetch) => Unset { waker, fetch },
    };
    match (state, event) {
        (Unset { fetch, .. }, FetcherEvent::Waker(waker)) => ready(Some(waker), fetch),
        (Ready { fetch, .. } | Running { fetch, .. }, FetcherEvent::Waker(waker)) => Ready { waker, fetch },
        (Unset { waker, .. }, FetcherEvent::Fetch(fetch)) => ready(waker, Some(fetch)),
        (Ready { waker, .. } | Running { waker, .. }, FetcherEvent::Fetch(fetch)) => Ready { waker, fetch },
        (Ready { waker, fetch }, FetcherEvent::Start(pool)) => Running { waker, fetch, pool },
        (state, FetcherEvent::Start(_)) => state,
    }
}

struct Landed {
    image: Image,
    bytes: Arc<[u8]>,
    size: Option<[u32; 2]>,
    weight: usize,
}

type FetchedEntry = (Source, Result<Landed, String>);

/// The pictures markdown in replies shows, shared by every reply's view (`Rc`, so a view's
/// closures can hold it).
#[derive(Default)]
pub struct MdStore {
    fetcher: RefCell<Fetcher>,
    cache: RefCell<Option<Arc<ImageCache>>>,
    tick: Cell<u64>,
    frame: Cell<u64>,
    slots: RefCell<HashMap<Source, Slot>>,
    /// What the readers fetched, already decoding (or a failure), waiting for the view.
    fetched: Arc<Mutex<Vec<FetchedEntry>>>,
    /// A fetch or decode finished since the host last polled: the view must run again.
    landed: Arc<AtomicBool>,
    /// Counts fetches and decodes that finished, for the rows that draw pictures to rebuild.
    version: Arc<AtomicU64>,
}

/// Image extensions Desktop reads (`runtime/server/image.ts`).
const IMAGE_TYPES: [&str; 9] = ["png", "jpeg", "jpg", "gif", "webp", "svg", "avif", "bmp", "ico"];

impl MdStore {
    pub fn set_waker(&self, waker: Waker) {
        self.send(FetcherEvent::Waker(waker));
        self.changed();
    }

    /// Where files and web pictures come from (the live server, the web); without them every
    /// image shows its alt text and every link its globe. Pictures that failed are asked for
    /// again, as the new server may have them.
    pub fn set_fetch(&self, files: FileFetch, web: WebFetch) {
        self.send(FetcherEvent::Fetch(Fetches { files, web }));
        self.slots.borrow_mut().retain(|_, slot| !matches!(slot, Slot::Failed(_)));
        self.changed();
    }

    /// Rows that showed alt text while pictures couldn't be fetched build again.
    fn changed(&self) {
        self.version.fetch_add(1, Ordering::AcqRel);
        self.landed.store(true, Ordering::Release);
    }

    fn send(&self, event: FetcherEvent) {
        let mut fetcher = self.fetcher.borrow_mut();
        *fetcher = step(std::mem::take(&mut *fetcher), event);
    }

    /// Changes whenever a picture lands or decodes: a memoized row that draws pictures keys on it.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Whether a picture landed since the last call; the host's `poll` returns it.
    pub fn poll(&self) -> bool {
        self.landed.swap(false, Ordering::AcqRel)
    }

    fn next_tick(&self) -> u64 {
        let t = self.tick.get().wrapping_add(1);
        self.tick.set(t);
        t
    }

    pub(super) fn begin_frame(&self) {
        let next = self.frame.get().wrapping_add(1).max(1);
        self.frame.set(next);
    }

    /// The picture for markdown image `src` in a reply of a session in `directory`, once it is
    /// fetching or decoding; `None` when there is none to show (an unknown type, a failed read).
    pub fn image(&self, directory: Option<&str>, src: &str) -> Option<Image> {
        self.get(image_source(directory, src)?, MD_MAX_PT)
    }

    /// Full-resolution image decoded on demand for the lightbox viewer without entering the
    /// markdown thumbnail LRU cache.
    pub(super) fn image_full(&self, directory: Option<&str>, src: &str) -> Option<Image> {
        let bytes = self.bytes(directory, src)?;
        Some(self.cache()?.load(bytes))
    }

    /// Header natural dimensions of markdown image `src`, if known.
    pub(super) fn natural_size(&self, directory: Option<&str>, src: &str) -> Option<[u32; 2]> {
        match self.slots.borrow().get(&image_source(directory, src)?)? {
            Slot::Decoding { size, .. } | Slot::Held { size, .. } => *size,
            Slot::Fetching | Slot::Failed(_) => None,
        }
    }

    fn failure(&self, source: &Source) -> Option<String> {
        match self.slots.borrow().get(source)? {
            Slot::Failed(reason) => Some(reason.clone()),
            Slot::Decoding { image, .. } => image.error().map(|e| e.to_string()),
            _ => None,
        }
    }

    pub(super) fn read_image(&self, directory: Option<&str>, src: &str) -> Result<Option<Image>, String> {
        let Some(source) = image_source(directory, src) else { return Ok(None) };
        let image = self.get(source.clone(), MD_MAX_PT);
        match self.failure(&source) {
            Some(reason) => Err(reason),
            None => Ok(image),
        }
    }

    /// Markdown image `src`'s encoded bytes, once they've landed.
    pub fn bytes(&self, directory: Option<&str>, src: &str) -> Option<Arc<[u8]>> {
        match self.slots.borrow().get(&image_source(directory, src)?)? {
            Slot::Decoding { bytes, .. } | Slot::Held { bytes, .. } => Some(bytes.clone()),
            Slot::Fetching | Slot::Failed(_) => None,
        }
    }

    /// The favicon to draw before a link to `url`, once it has decoded: Google's favicon
    /// service for a public host, as Desktop asks; `None` for GitHub (its mark), private and
    /// local hosts, and until it loads.
    pub fn favicon(&self, url: &str) -> Option<Pixels> {
        let host = favicon_host(url)?;
        let image = self.get(Source::Web(format!("https://www.google.com/s2/favicons?domain={}&sz=32", encode(&host))), FAVICON_MAX_PT)?;
        (image.is_ready() && image.error().is_none() && image.size().is_some_and(|[w, _]| w > 0)).then(|| image.pixels().clone())
    }

    /// Site `host`'s own favicon, as the Sources experiment's site icons ask the site for it
    /// (`site-icon.tsx`: `https://<host>/favicon.ico`), and where it stands. A favicon that failed
    /// stays failed until the fetches change, so later tiles of its host show the letter at once.
    pub fn site_icon(&self, host: &str) -> SiteIcon {
        let source = Source::Web(format!("https://{host}/favicon.ico"));
        let image = self.get(source.clone(), FAVICON_MAX_PT);
        if matches!(self.slots.borrow().get(&source), Some(Slot::Failed(_))) {
            return SiteIcon::Failed;
        }
        match image {
            Some(image) if image.is_ready() => match (image.error(), image.size()) {
                // As Desktop checks `naturalWidth > 0`.
                (None, Some([w, h])) if w > 0 && h > 0 => SiteIcon::Loaded(image.pixels().clone()),
                _ => SiteIcon::Failed,
            },
            _ => SiteIcon::Loading,
        }
    }

    fn get(&self, source: Source, max_pt: f32) -> Option<Image> {
        self.land();
        let now = self.next_tick();
        let cur_frame = self.frame.get();
        let held = match self.slots.borrow_mut().get_mut(&source) {
            Some(Slot::Decoding { image, last_used, last_frame, .. }) => {
                *last_used = now;
                *last_frame = cur_frame;
                return Some(image.clone());
            }
            Some(Slot::Fetching | Slot::Failed(_)) => return None,
            Some(Slot::Held { bytes, size, .. }) => Some((bytes.clone(), *size)),
            None => None,
        };
        if let Some((bytes, size)) = held {
            let scale = display_scale();
            let eff_pt = md_effective_pt(size, max_pt);
            let cache = self.cache()?;
            let (image, max_px) = load_thumb(&cache, bytes.clone(), source_mime(&source), size, eff_pt, scale);
            let weight = scaled_weight(size, max_px);
            self.slots.borrow_mut().insert(source.clone(), Slot::Decoding { image: image.clone(), bytes, size, weight, last_used: now, last_frame: cur_frame });
            self.trim_pinned(MD_PIXEL_BUDGET, MD_BYTES_BUDGET, Some(&source));
            return Some(image);
        }
        let scale = display_scale();
        match self.enqueue(source.clone(), max_pt, scale) {
            Enqueue::Unready => {}
            Enqueue::Queued => { self.slots.borrow_mut().insert(source, Slot::Fetching); }
            Enqueue::Failed => { self.slots.borrow_mut().insert(source, Slot::Failed("Image reader stopped".into())); }
        }
        None
    }

    /// Drops all decoded CPU pixels, keeping encoded bytes within `MD_BYTES_BUDGET`.
    pub fn rest(&self) {
        self.land();
        for slot in self.slots.borrow_mut().values_mut() {
            if let Slot::Decoding { image, bytes, size, last_used, .. } = slot {
                let sz = (*size).or_else(|| image.size());
                *slot = Slot::Held { bytes: bytes.clone(), size: sz, last_used: *last_used };
            }
        }
        self.trim_pinned(0, MD_BYTES_BUDGET, None);
    }

    pub(super) fn trim(&self, pixel_budget: usize, bytes_budget: usize) {
        self.trim_pinned(pixel_budget, bytes_budget, None);
    }

    /// Enforces LRU budgets on decoded CPU pixels and retained encoded bytes, never evicting
    /// `pin` or entries touched in the current render frame.
    fn trim_pinned(&self, pixel_budget: usize, bytes_budget: usize, pin: Option<&Source>) {
        let mut slots = self.slots.borrow_mut();
        let mut pixels: usize = slots
            .values()
            .map(|s| match s { Slot::Decoding { image, weight, .. } => live_weight(image, *weight), _ => 0 })
            .sum();
        let cur_frame = (self.frame.get() > 0).then(|| self.frame.get());
        while pixels > pixel_budget {
            let oldest = slots
                .iter()
                .filter(|(k, _)| pin != Some(*k))
                .filter_map(|(k, s)| match s {
                    Slot::Decoding { last_used, last_frame, .. } if cur_frame != Some(*last_frame) => Some((k.clone(), *last_used)),
                    _ => None,
                })
                .min_by_key(|(_, t)| *t)
                .map(|(k, _)| k);
            let Some(k) = oldest else { break };
            if let Some(slot) = slots.get_mut(&k)
                && let Slot::Decoding { image, bytes, size, weight, last_used, .. } = slot
            {
                pixels = pixels.saturating_sub(live_weight(image, *weight));
                let sz = (*size).or_else(|| image.size());
                *slot = Slot::Held { bytes: bytes.clone(), size: sz, last_used: *last_used };
            }
        }
        let mut bytes_total: usize = slots
            .values()
            .map(|s| match s { Slot::Decoding { bytes, .. } | Slot::Held { bytes, .. } => bytes.len(), _ => 0 })
            .sum();
        while bytes_total > bytes_budget {
            let oldest = slots
                .iter()
                .filter(|(k, _)| pin != Some(*k))
                .filter_map(|(k, s)| match s {
                    Slot::Held { last_used, .. } => Some((k.clone(), false, *last_used)),
                    Slot::Decoding { last_used, last_frame, .. } if cur_frame != Some(*last_frame) => Some((k.clone(), true, *last_used)),
                    _ => None,
                })
                .min_by_key(|(_, active, t)| (*active, *t))
                .map(|(k, _, _)| k);
            let Some(k) = oldest else { break };
            if let Some(removed) = slots.remove(&k)
                && let Slot::Decoding { bytes, .. } | Slot::Held { bytes, .. } = removed
            {
                bytes_total = bytes_total.saturating_sub(bytes.len());
            }
        }
    }

    /// Takes what the readers fetched: each picture already decoding.
    fn land(&self) {
        let fetched = std::mem::take(&mut *self.fetched.lock().unwrap_or_else(|e| e.into_inner()));
        if fetched.is_empty() {
            return;
        }
        let now = self.next_tick();
        let cur_frame = self.frame.get();
        let mut last = None;
        for (source, read) in fetched {
            let slot = match read {
                Ok(l) => Slot::Decoding { image: l.image, bytes: l.bytes, size: l.size, weight: l.weight, last_used: now, last_frame: cur_frame },
                Err(reason) => Slot::Failed(reason),
            };
            last = Some(source.clone());
            self.slots.borrow_mut().insert(source, slot);
        }
        self.trim_pinned(MD_PIXEL_BUDGET, MD_BYTES_BUDGET, last.as_ref());
    }

    fn cache(&self) -> Option<Arc<ImageCache>> {
        if self.cache.borrow().is_none() {
            let waker = match &*self.fetcher.borrow() {
                Fetcher::Unset { waker, .. } => waker.clone()?,
                Fetcher::Ready { waker, .. } | Fetcher::Running { waker, .. } => waker.clone(),
            };
            let (landed, version) = (self.landed.clone(), self.version.clone());
            *self.cache.borrow_mut() = Some(Arc::new(ImageCache::new(Arc::new(move || {
                version.fetch_add(1, Ordering::AcqRel);
                landed.store(true, Ordering::Release);
                waker();
            }))));
        }
        self.cache.borrow().clone()
    }

    /// Queues `source` into a bounded worker pool (`MAX_READERS` concurrent fetches/decodes) so
    /// replies with dozens of markdown images never spike RAM with unbounded parallel reads.
    fn enqueue(&self, source: Source, max_pt: f32, scale: f32) -> Enqueue {
        let state = match &*self.fetcher.borrow() {
            Fetcher::Running { fetch, waker, pool } => Some((fetch.clone(), waker.clone(), Some(pool.clone()))),
            Fetcher::Ready { fetch, waker } => Some((fetch.clone(), waker.clone(), None)),
            Fetcher::Unset { .. } => None,
        };
        let Some((fetch, waker, existing)) = state else { return Enqueue::Unready };
        let pool = match existing {
            Some(pool) => pool,
            None => {
                let pool = Arc::new(Mutex::new((VecDeque::new(), 0usize)));
                self.send(FetcherEvent::Start(pool.clone()));
                pool
            }
        };
        struct ActiveGuard { pool: ReadPool, armed: bool }
        impl Drop for ActiveGuard {
            fn drop(&mut self) {
                if self.armed {
                    let mut g = self.pool.lock().unwrap_or_else(|e| e.into_inner());
                    g.1 = g.1.saturating_sub(1);
                }
            }
        }
        let Some(cache) = self.cache() else { return Enqueue::Unready };
        let key_for_rollback = source.clone();
        let mut p = pool.lock().unwrap_or_else(|e| e.into_inner());
        p.0.push_back((source, max_pt, scale));
        if p.1 >= MAX_READERS {
            return Enqueue::Queued;
        }
        p.1 += 1;
        drop(p);
        let (fetched, landed, version) = (self.fetched.clone(), self.landed.clone(), self.version.clone());
        let pool_w = pool.clone();
        if rune::task::spawn("markdown-media-read", move || {
            let mut active = ActiveGuard { pool: pool_w.clone(), armed: true };
            loop {
                let next = {
                    let mut guard = pool_w.lock().unwrap_or_else(|e| e.into_inner());
                    match guard.0.pop_front() {
                        Some(item) => item,
                        None => {
                            guard.1 = guard.1.saturating_sub(1);
                            active.armed = false;
                            return;
                        }
                    }
                };
                let (source, max_pt, scale) = next;
                let bytes = match &source {
                    Source::File { directory, path } => (fetch.files)(directory, path),
                    Source::Web(url) => (fetch.web)(url),
                };
                let read = bytes.map(|bytes| {
                    let bytes: Arc<[u8]> = bytes.into();
                    let size = dimensions(&bytes);
                    let eff_pt = md_effective_pt(size, max_pt);
                    let (image, max_px) = load_thumb(&cache, bytes.clone(), source_mime(&source), size, eff_pt, scale);
                    let weight = scaled_weight(size, max_px);
                    Landed { image, bytes, size, weight }
                });
                fetched.lock().unwrap_or_else(|e| e.into_inner()).push((source, read));
                version.fetch_add(1, Ordering::AcqRel);
                landed.store(true, Ordering::Release);
                waker();
            }
        })
        .is_ok()
        {
            return Enqueue::Queued;
        }
        let mut p = pool.lock().unwrap_or_else(|e| e.into_inner());
        p.1 = p.1.saturating_sub(1);
        if p.1 == 0 {
            if let Some(pos) = p.0.iter().rposition(|(s, _, _)| *s == key_for_rollback) {
                p.0.remove(pos);
            }
            Enqueue::Failed
        } else {
            Enqueue::Queued
        }
    }
}

/// What image `src` reads: a public web URL as is; a verified local path (or `file:` URL)
/// through the server, in its own directory when absolute, else in the session's. Refuses raw
/// UNC/device paths and non-public web URLs before enqueuing any zero-click fetch.
fn image_source(directory: Option<&str>, src: &str) -> Option<Source> {
    let raw = src.trim();
    if raw.is_empty() || browser_policy::is_raw_unc_href(raw) {
        return None;
    }
    let value = raw.replace(['\\'], "/").replace("%5C", "/").replace("%5c", "/");
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return browser_policy::is_public_http_url(raw).then(|| Source::Web(raw.to_owned()));
    }
    if value.is_empty() || value.starts_with("//") || value.chars().any(char::is_control) {
        return None;
    }
    let path = match lower.strip_prefix("file://") {
        Some(_) => {
            let rest = &value[7..];
            let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return None;
            }
            let path = format!("/{path}");
            // `file:///C:/x` names a drive.
            if path.len() > 3 && path.as_bytes()[2] == b':' { path[1..].to_owned() } else { path }
        }
        None => value,
    };
    let path = decode(&path)?;
    if browser_policy::is_raw_unc_href(&path) || browser_policy::is_unc_or_device_path(&path) {
        return None;
    }
    let drive = path.len() > 2 && path.as_bytes()[1] == b':' && path.as_bytes()[0].is_ascii_alphabetic() && matches!(path.as_bytes()[2], b'/' | b'\\');
    if !drive && path.split_once(':').is_some_and(|(scheme, _)| !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))) {
        return None;
    }
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    if !IMAGE_TYPES.contains(&ext.as_str()) {
        return None;
    }
    if drive || path.starts_with('/') {
        let (dir, name) = path.rsplit_once('/')?;
        let dir = if dir.is_empty() || dir.ends_with(':') { format!("{dir}/") } else { dir.to_owned() };
        return Some(Source::File { directory: dir, path: name.to_owned() });
    }
    Some(Source::File { directory: directory?.to_owned(), path })
}

/// The host to ask a favicon for: a public name, not GitHub's (it keeps its mark), nor a
/// reserved, local, credentialed, or private one (`publicFaviconHost`, `privateFaviconSuffix`).
fn favicon_host(url: &str) -> Option<String> {
    if !browser_policy::is_public_http_url(url) {
        return None;
    }
    let rest = url.split_once("://").filter(|(s, _)| s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https"))?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.split(':').next()?.to_ascii_lowercase();
    if host == "github.com" || host == "home.arpa" || host.ends_with(".home.arpa") {
        return None;
    }
    const PRIVATE: [&str; 9] = [".alt", ".example", ".internal", ".invalid", ".local", ".localhost", ".onion", ".test", ".ts.net"];
    if PRIVATE.iter().any(|s| host.ends_with(s)) {
        return None;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let tld = labels.last()?;
    let public = labels.len() >= 2
        && (2..=63).contains(&tld.len())
        && tld.chars().all(|c| c.is_ascii_lowercase())
        && labels.iter().all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    // Desktop asks with the URL's host, port included.
    public.then(|| authority.to_ascii_lowercase())
}

fn encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let v = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16).ok()?;
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok().filter(|p| !p.chars().any(char::is_control) && !p.starts_with("//"))
}

/// What a reply's markdown needs to show pictures: the store and the session's directory.
#[derive(Clone)]
pub struct MdMedia {
    pub store: Rc<MdStore>,
    pub directory: Option<Rc<str>>,
    /// The image the viewer shows now (its matched id), so its place in the reply holds empty
    /// while it flies.
    pub viewing: Option<String>,
    pub flight: rune::Spring,
    /// The message the markdown is, whose items follow an image's in its right-click menu.
    pub message: Option<Rc<str>>,
    /// Where the markdown shows (its row's key), which scopes its images' shared-element ids.
    pub scope: Option<Rc<str>>,
}
