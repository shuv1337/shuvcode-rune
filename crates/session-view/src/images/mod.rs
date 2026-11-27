//! Media on attachments: `media:` ref fetches on a worker thread, scaled thumbnail handles, and
//! a byte-budgeted LRU cache behind a transcript's thumbnails.

pub mod bento;
mod cache;
pub mod drag;
pub mod markdown;
pub mod menu;
pub mod viewer;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use rune::{El, Image, ImageCache, SurfaceFit, TextStyle, Waker, col, el, row, text};

use opencode_kit::model::{Attachment, Message, Transcript};

use crate::TimelineHost;
use crate::focus::FocusExt;
use crate::palette::Pal;

pub use cache::Media;
use cache::{
    MAX_META_ENTRIES, PIXEL_BUDGET, REF_BYTES_BUDGET, REST_PIXEL_BUDGET, RefSlot, THUMB_MAX_PT,
    cap_map, dimensions, display_scale, load_thumb, scaled_weight, target_px,
};
pub use drag::{Dragged, draggable};
pub use viewer::{Strip, Viewing, opens, poll, shown, view_image, viewer};

/// Newest message window checked by `transcript_ready` (matches `session_pages::LATEST`).
pub const LATEST: usize = 12;

const SMALL: TextStyle = TextStyle::new(12.0, 440.0).lh(16.0);

/// `outline-photo` (`icons/catalog.ts`), shown inside a failed image tile
/// (`packages/session-ui/src/tools/tool-result-images.tsx:86-93`, `message-content.tsx`).
pub(crate) static OUTLINE_PHOTO: rune::Icon = rune::Icon {
    name: "outline-photo",
    view: 14.0,
    body: r#"<path d="M11.6666 11.6666L8.16663 8.16663L6.99997 9.33332L4.66663 6.99998L2.1583 9.50832M2.04163 2.04165H11.9583V11.9583H2.04163V2.04165ZM9.33332 5.24998C9.33332 5.81377 8.87629 6.27081 8.31244 6.27081C7.74866 6.27081 7.29163 5.81377 7.29163 5.24998C7.29163 4.68619 7.74866 4.22915 8.31244 4.22915C8.87629 4.22915 9.33332 4.68619 9.33332 5.24998Z" stroke="currentColor" stroke-linecap="square"/>"#,
};

/// Decoded image cache, `media:` ref fetcher, and active image lightbox state.
#[derive(Default)]
pub struct MediaStore {
    pub(crate) videos: crate::video::Clips,
    /// The image open in the viewer (decoded at full resolution on demand).
    pub viewing: Option<Viewing>,
    /// The open image's zoom and pan.
    pub zoom: rune_kit::ImageZoom,
    pub waker: Option<Waker>,
    cache: RefCell<Option<Arc<ImageCache>>>,
    tick: Cell<u64>,
    frame: Cell<u64>,
    /// Decoded thumbnail handles by the bytes' address, bounded by `PIXEL_BUDGET` LRU.
    images: RefCell<HashMap<(usize, usize), InlineEntry>>,
    /// `media:<hash>` refs: fetched once, decoded to thumbnail size, bounded by LRU budgets.
    pub media: Media,
    refs: RefCell<HashMap<String, RefSlot>>,
    /// A ref's header dimensions, so its box keeps its shape even after byte eviction.
    ref_sizes: RefCell<HashMap<String, [u32; 2]>>,
    /// SHA-256 (a ref's hash) of inline images, by image id, computed only when a ref misses.
    shas: RefCell<HashMap<rune::ImageId, String>>,
    /// Pictures for markdown in replies: images and link favicons.
    pub markdown: std::rc::Rc<markdown::MdStore>,
}

pub(super) struct InlineEntry {
    pub(super) prepared: PreparedImage,
    pub(super) last_used: u64,
    pub(super) last_frame: u64,
}

impl InlineEntry {
    pub(super) fn weight(&self) -> usize {
        cache::live_weight(&self.prepared.image, self.prepared.weight())
    }
}

/// Header dimensions are available before decode; the same handle crosses from prefetch to view.
#[derive(Clone)]
pub struct PreparedImage {
    bytes: Arc<[u8]>,
    pub image: Image,
    size: Option<[u32; 2]>,
    max_px: u32,
}

impl PreparedImage {
    fn new(cache: &ImageCache, bytes: Arc<[u8]>, mime: &str, max_pt: f32, scale: f32) -> Self {
        let size = dimensions(&bytes);
        let (image, max_px) = load_thumb(cache, bytes.clone(), mime, size, max_pt, scale);
        Self { bytes, image, size, max_px }
    }

    pub fn handle(&self) -> Image {
        self.image.clone()
    }

    pub fn weight(&self) -> usize {
        // Scaled RGBA plus its mip chain; encoded bytes are counted separately.
        scaled_weight(self.size, self.max_px)
    }

    pub fn wait(&self) {
        self.image.wait().ok();
    }
}

/// Called only on the page worker. Newest attachments first, decoded at thumbnail scale and
/// bounded so page prefetch never spikes CPU RAM; older history stays demand-loaded.
pub fn prepare_images(messages: &[Message], cache: &ImageCache) -> Vec<PreparedImage> {
    use opencode_kit::model::{Part, user_files};
    let scale = display_scale();
    let max_px = target_px(THUMB_MAX_PT, scale);
    let mut prepared = Vec::new();
    let mut weight = 0usize;
    let mut add = |f: &Attachment| {
        if !decodable(&f.mime) || f.media.is_some() || prepared.len() >= 6 {
            return;
        }
        let size = dimensions(&f.bytes);
        let w = scaled_weight(size, max_px);
        if weight + w > 12 << 20 {
            return;
        }
        weight += w;
        prepared.push(PreparedImage::new(cache, f.bytes.clone(), &f.mime, THUMB_MAX_PT, scale));
    };
    for m in messages.iter().rev() {
        match m {
            Message::User { id, .. } => {
                for f in user_files(id).iter().rev() {
                    add(f);
                }
            }
            Message::Assistant(a) => {
                for p in a.parts.iter().rev() {
                    if let Part::Tool { files, .. } = p {
                        for f in files.iter().rev() {
                            add(f);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    prepared
}

/// How many decodable images a page carries inline, and their encoded bytes.
pub fn inline_images(messages: &[Message]) -> (usize, usize) {
    use opencode_kit::model::{Part, user_files};
    let (mut count, mut bytes) = (0, 0);
    let mut add = |f: &Attachment| {
        if decodable(&f.mime) && f.media.is_none() {
            count += 1;
            bytes += f.bytes.len();
        }
    };
    for m in messages {
        match m {
            Message::User { id, .. } => user_files(id).iter().for_each(&mut add),
            Message::Assistant(a) => {
                for p in &a.parts {
                    if let Part::Tool { files, .. } = p {
                        files.iter().for_each(&mut add);
                    }
                }
            }
            _ => {}
        }
    }
    (count, bytes)
}

/// Reads a `media:` ref's bytes (`Server::media` live; a stub in tests).
pub type Fetch = Arc<dyn Fn(&str) -> Result<Vec<u8>, String> + Send + Sync>;

/// Where a ref's bytes are.
#[derive(Clone, Debug, PartialEq)]
pub enum Fetched {
    Loading,
    Ready(Arc<[u8]>),
    Failed(String),
    /// Handed to the image cache, which holds the pixels now.
    Decoded,
}

/// What the cache decodes (the image types Desktop's `<img>` shows, `util/src/artifact.ts`);
/// other types show as a named chip. GIF, WebP and APNG animate.
pub fn decodable(mime: &str) -> bool {
    matches!(
        mime,
        "image/png"
            | "image/apng"
            | "image/jpeg"
            | "image/jpg"
            | "image/gif"
            | "image/webp"
            | "image/avif"
            | "image/bmp"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
            | "image/svg+xml"
    )
}

impl MediaStore {
    fn next_tick(&self) -> u64 {
        let t = self.tick.get().wrapping_add(1);
        self.tick.set(t);
        t
    }

    /// Advances the render-frame epoch once per view pass so eviction never drops images touched
    /// in the current frame.
    pub fn begin_frame(&self) {
        let next = self.frame.get().wrapping_add(1).max(1);
        self.frame.set(next);
        self.markdown.begin_frame();
    }

    /// Shared `rune::ImageCache` for this store and the server's page workers so identical
    /// attachment bytes decode once across prefetch and timeline (`MdStore` keeps its own cache
    /// with a version-bumping waker for markdown rows).
    pub fn image_cache(&self) -> Arc<ImageCache> {
        let mut cache = self.cache.borrow_mut();
        cache
            .get_or_insert_with(|| {
                Arc::new(ImageCache::new(self.waker.clone().unwrap_or_else(|| Arc::new(|| {}))))
            })
            .clone()
    }

    pub fn set_waker(&mut self, waker: Waker) {
        self.waker = Some(waker.clone());
        *self.cache.borrow_mut() = Some(Arc::new(ImageCache::new(waker.clone())));
        self.markdown.set_waker(waker);
    }

    /// Whether a ref's bytes landed since the last call. The host's `poll` returns it, so a ref
    /// still showing its loading box turns into its image without waiting for input.
    pub fn poll(&self) -> bool {
        self.media.landed.swap(false, Ordering::AcqRel) | self.markdown.poll()
    }

    /// A story controls byte arrival with the replay clock, then uses the real decoder.
    pub fn fixture_media(&self, hash: &str, bytes: Option<Vec<u8>>) {
        let ready = bytes.is_some();
        self.media
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(hash.into(), bytes.map_or(Fetched::Loading, |b| Fetched::Ready(b.into())));
        if ready {
            let f = Attachment { mime: "image/png".into(), media: Some(hash.into()), ..Default::default() };
            if let Some(img) = self.image(&f) {
                img.wait().ok();
            }
        }
    }

    /// Start uncached/ref reads in the newest message window too. Older history stays
    /// demand-loaded; preparing a page is not the same as finishing its image decodes.
    pub fn transcript_ready(&self, t: &Transcript) -> bool {
        use opencode_kit::model::{Part, user_files};
        let mut ready = true;
        let mut check = |f: &Attachment| {
            if !decodable(&f.mime) {
                return;
            }
            let loaded = self.image(f).is_some_and(|i| i.is_ready())
                || matches!(self.fetched(f), Some(Fetched::Failed(_)));
            ready &= loaded;
        };
        for message in t.messages.iter().rev().take(LATEST) {
            match message {
                Message::User { id, .. } => {
                    for file in user_files(id).iter() {
                        check(file);
                    }
                }
                Message::Assistant(a) => {
                    for part in &a.parts {
                        if let Part::Tool { files, .. } = part {
                            for file in files {
                                check(file);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        ready
    }

    pub fn image_handles(&self) -> Vec<Image> {
        let inline = self.images.borrow().values().map(|e| e.prepared.image.clone()).collect::<Vec<_>>();
        let refs = self.refs.borrow().values().filter_map(|slot| match slot {
            RefSlot::Decoded { image, .. } => Some(image.clone()),
            RefSlot::Held { .. } => None,
        }).collect::<Vec<_>>();
        inline.into_iter().chain(refs).collect()
    }

    /// Enforces active-session LRU budgets without shrinking below active viewport budgets
    /// (used by quiet periodic sweeps while tabs are actively viewed).
    pub fn trim_active(&self) {
        self.images.borrow_mut().retain(|_, e| Arc::strong_count(&e.prepared.bytes) > 1);
        self.evict_pixels(PIXEL_BUDGET, cache::Pin::None);
        self.evict_ref_bytes(REF_BYTES_BUDGET, None);
        self.media.prune_orphaned_ready(&self.refs.borrow());
        self.markdown.trim(markdown::MD_PIXEL_BUDGET, markdown::MD_BYTES_BUDGET);
    }

    /// Drops orphaned inline entries and trims decoded pixels + encoded ref bytes down to rest
    /// budgets, regardless of how many background tabs remain open.
    pub fn rest(&self) {
        self.images.borrow_mut().retain(|_, e| Arc::strong_count(&e.prepared.bytes) > 1);
        self.evict_pixels(REST_PIXEL_BUDGET, cache::Pin::None);
        self.evict_ref_bytes(REF_BYTES_BUDGET, None);
        self.media.prune_orphaned_ready(&self.refs.borrow());
        self.markdown.trim(markdown::MD_REST_PIXEL_BUDGET, markdown::MD_BYTES_BUDGET);
    }

    /// Called on tab switch or shown-tab close: closes any open lightbox (releasing its
    /// full-resolution handle) and enforces active budgets without evicting the destination tab's
    /// thumbnails before it renders.
    pub fn on_tab_switch(&mut self) {
        self.viewing = None;
        self.trim_active();
    }

    /// Drops all decoded CPU pixels across inline attachments, `media:` refs, and markdown
    /// pictures, keeping encoded bytes within budget so re-opening decodes without refetching.
    pub fn rest_pixels(&self) {
        self.images.borrow_mut().clear();
        let mut refs = self.refs.borrow_mut();
        let mut state = self.media.state.lock().unwrap_or_else(|e| e.into_inner());
        for (hash, slot) in refs.iter_mut() {
            if slot.demote() > 0 {
                state.insert(hash.clone(), Fetched::Ready(slot.bytes().clone()));
            }
        }
        drop(state);
        drop(refs);
        self.evict_ref_bytes(REF_BYTES_BUDGET, None);
        self.media.prune_orphaned_ready(&self.refs.borrow());
        self.markdown.rest();
    }

    pub fn adopt(&self, prepared: &[PreparedImage]) {
        let mut images = self.images.borrow_mut();
        images.retain(|_, e| Arc::strong_count(&e.prepared.bytes) > 1);
        let now = self.next_tick();
        for p in prepared {
            let mut p = p.clone();
            if let Some(old) = images.values().find(|old| old.prepared.image.id() == p.image.id() && old.prepared.image.is_ready()) {
                p.image = old.prepared.image.clone();
                p.max_px = old.prepared.max_px;
            }
            let key = (p.bytes.as_ptr() as usize, p.bytes.len());
            images.insert(key, InlineEntry { prepared: p, last_used: now, last_frame: self.frame.get() });
        }
        drop(images);
        self.evict_pixels(PIXEL_BUDGET, cache::Pin::None);
    }

    /// The image's own shape at most 320×240 points (2× pixels). A ref's shape is known once its
    /// bytes land; until then it holds a 160×120 box.
    pub fn thumbnail_size(&self, f: &Attachment) -> (f32, f32) {
        let size = match &f.media {
            Some(hash) => self
                .ref_sizes
                .borrow()
                .get(hash)
                .copied()
                .or_else(|| self.refs.borrow().get(hash).and_then(|s| s.size())),
            None => {
                let key = (f.bytes.as_ptr() as usize, f.bytes.len());
                self.images
                    .borrow()
                    .get(&key)
                    .and_then(|e| e.prepared.size.or_else(|| e.prepared.image.size()))
                    .or_else(|| dimensions(&f.bytes))
            }
        };
        size.map_or((160.0, 120.0), |[w, h]| fit(w as f32 / 2.0, h as f32 / 2.0, 320.0, 240.0))
    }

    /// Natural pixel dimensions from the image header, if known, for the lightbox viewer.
    pub(crate) fn natural_size(&self, f: &Attachment) -> Option<[u32; 2]> {
        match &f.media {
            Some(hash) => self
                .ref_sizes
                .borrow()
                .get(hash)
                .copied()
                .or_else(|| self.refs.borrow().get(hash).and_then(|s| s.size())),
            None => {
                let key = (f.bytes.as_ptr() as usize, f.bytes.len());
                self.images
                    .borrow()
                    .get(&key)
                    .and_then(|e| e.prepared.size)
                    .or_else(|| dimensions(&f.bytes))
            }
        }
    }

    /// Where `media:` refs' bytes come from.
    pub fn set_fetch(&mut self, fetch: Fetch) {
        self.media.set_fetch(fetch);
    }

    /// Decoded thumbnail handle for `a` at default gallery scale (`THUMB_MAX_PT`).
    pub fn image(&self, a: &Attachment) -> Option<Image> {
        self.image_sized(a, THUMB_MAX_PT)
    }

    /// Decoded thumbnail handle for `a`, scaled to fit `max_pt` logical points at the current
    /// display scale so CPU RAM never holds full 4K/5K RGBA mip chains for small timeline tiles.
    pub fn image_sized(&self, a: &Attachment, max_pt: f32) -> Option<Image> {
        if !decodable(&a.mime) {
            return None;
        }
        let scale = display_scale();
        let wanted_px = target_px(max_pt, scale);
        let now = self.next_tick();
        let cur_frame = self.frame.get();

        if let Some(hash) = &a.media {
            let hit = {
                let mut refs = self.refs.borrow_mut();
                refs.get_mut(hash).map(|slot| match slot {
                    RefSlot::Decoded { image, max_px, last_used, last_frame, .. } if *max_px >= wanted_px => {
                        *last_used = now;
                        *last_frame = cur_frame;
                        (image.clone(), false)
                    }
                    RefSlot::Decoded { bytes, size, .. } | RefSlot::Held { bytes, size, .. } => {
                        let bytes = bytes.clone();
                        let size = *size;
                        let cache = self.image_cache();
                        let (img, max_px) = load_thumb(&cache, bytes.clone(), &a.mime, size, max_pt, scale);
                        let weight = scaled_weight(size, max_px);
                        *slot = RefSlot::Decoded { image: img.clone(), bytes, size, max_px, weight, last_used: now, last_frame: cur_frame };
                        self.media.state.lock().unwrap_or_else(|e| e.into_inner()).insert(hash.clone(), Fetched::Decoded);
                        (img, true)
                    }
                })
            };
            if let Some((img, decoded)) = hit {
                if decoded {
                    self.evict_pixels(PIXEL_BUDGET, cache::Pin::Ref(hash));
                }
                return Some(img);
            }
            if let Some(p) = self.inline_copy(hash) {
                // Upgrade composer-sized thumbnails (160 pt) to timeline resolution when adopted.
                let (img, max_px, weight) = if p.max_px >= wanted_px {
                    (p.image.clone(), p.max_px, p.weight())
                } else {
                    let cache = self.image_cache();
                    let (img, max_px) = load_thumb(&cache, p.bytes.clone(), &a.mime, p.size, max_pt, scale);
                    (img, max_px, scaled_weight(p.size, max_px))
                };
                self.adopt_ref(hash, img.clone(), p.size, p.bytes.clone(), max_px, weight, now);
                self.evict_pixels(PIXEL_BUDGET, cache::Pin::Ref(hash));
                self.evict_ref_bytes(REF_BYTES_BUDGET, Some(hash));
                return Some(img);
            }
            let Fetched::Ready(bytes) = self.media.get(hash, self.waker.as_ref()) else {
                return None;
            };
            let size = dimensions(&bytes);
            let cache = self.image_cache();
            let (img, max_px) = load_thumb(&cache, bytes.clone(), &a.mime, size, max_pt, scale);
            self.adopt_ref(hash, img.clone(), size, bytes, max_px, scaled_weight(size, max_px), now);
            self.evict_pixels(PIXEL_BUDGET, cache::Pin::Ref(hash));
            self.evict_ref_bytes(REF_BYTES_BUDGET, Some(hash));
            return Some(img);
        }

        let key = (a.bytes.as_ptr() as usize, a.bytes.len());
        if let Some(entry) = self.images.borrow_mut().get_mut(&key)
            && entry.prepared.max_px >= wanted_px
        {
            entry.last_used = now;
            entry.last_frame = cur_frame;
            return Some(entry.prepared.image.clone());
        }
        let cache = self.image_cache();
        let prepared = PreparedImage::new(&cache, a.bytes.clone(), &a.mime, max_pt, scale);
        let img = prepared.image.clone();
        self.images.borrow_mut().insert(key, InlineEntry { prepared, last_used: now, last_frame: cur_frame });
        self.evict_pixels(PIXEL_BUDGET, cache::Pin::Inline(key));
        Some(img)
    }

    /// Decodes `a` at full native resolution on demand for the lightbox viewer without inserting it
    /// into the thumbnail LRU cache, so closing the viewer releases full-res CPU pixels immediately.
    pub(crate) fn image_full(&self, a: &Attachment) -> Option<Image> {
        if !decodable(&a.mime) {
            return None;
        }
        let bytes = self.bytes(a)?;
        Some(self.image_cache().load(bytes))
    }

    #[allow(clippy::too_many_arguments)]
    fn adopt_ref(&self, hash: &str, img: Image, size: Option<[u32; 2]>, bytes: Arc<[u8]>, max_px: u32, weight: usize, now: u64) {
        self.refs.borrow_mut().insert(hash.to_owned(), RefSlot::Decoded { image: img, bytes, size, max_px, weight, last_used: now, last_frame: self.frame.get() });
        if let Some(size) = size {
            let mut sizes = self.ref_sizes.borrow_mut();
            cap_map(&mut sizes);
            sizes.insert(hash.to_owned(), size);
        }
        self.media.state.lock().unwrap_or_else(|e| e.into_inner()).insert(hash.to_owned(), Fetched::Decoded);
    }

    /// Evicts least-recently-used decoded thumbnail handles until total pixel weight fits `budget`.
    fn evict_pixels(&self, budget: usize, pin: cache::Pin<'_>) {
        let cur_frame = (self.frame.get() > 0).then(|| self.frame.get());
        let demoted = cache::evict_pixels(&mut self.images.borrow_mut(), &mut self.refs.borrow_mut(), budget, pin, cur_frame);
        if !demoted.is_empty() {
            let mut state = self.media.state.lock().unwrap_or_else(|e| e.into_inner());
            for (hash, bytes) in demoted {
                state.insert(hash, Fetched::Ready(bytes));
            }
        }
    }

    /// Evicts oldest encoded `media:` ref bytes once total cached ref bytes exceed `budget`.
    fn evict_ref_bytes(&self, budget: usize, pin: Option<&str>) {
        let cur_frame = (self.frame.get() > 0).then(|| self.frame.get());
        for hash in cache::evict_ref_bytes(&mut self.refs.borrow_mut(), budget, pin, cur_frame) {
            self.media.forget(&hash);
        }
    }

    /// An inline image already held whose bytes are the ref's (the server's hash is SHA-256).
    /// Each image is hashed once, and only when a ref misses.
    fn inline_copy(&self, hash: &str) -> Option<PreparedImage> {
        use sha2::{Digest, Sha256};
        let images = self.images.borrow();
        let mut shas = self.shas.borrow_mut();
        if shas.len() > MAX_META_ENTRIES {
            shas.clear();
        }
        images
            .values()
            .find(|e| shas.entry(e.prepared.image.id()).or_insert_with(|| format!("{:x}", Sha256::digest(&e.prepared.bytes))) == hash)
            .map(|e| e.prepared.clone())
    }

    /// An attachment's encoded bytes: inline ones as they are, a ref's once they've landed.
    pub fn bytes(&self, a: &Attachment) -> Option<Arc<[u8]>> {
        match &a.media {
            Some(hash) => self.refs.borrow().get(hash).map(|s| s.bytes().clone()),
            None => (!a.bytes.is_empty()).then(|| a.bytes.clone()),
        }
    }

    /// Forgets a ref's failed fetch, so the next `image` asks for its bytes again (Desktop's
    /// `retry.requested`).
    pub fn retry(&self, a: &Attachment) {
        let Some(hash) = &a.media else { return };
        let mut state = self.media.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(state.get(hash), Some(Fetched::Failed(_))) {
            state.remove(hash);
        }
    }

    /// A ref's fetch, once `image` has asked for it (None for inline files).
    pub fn fetched(&self, a: &Attachment) -> Option<Fetched> {
        let hash = a.media.as_ref()?;
        self.media.state.lock().unwrap_or_else(|e| e.into_inner()).get(hash).cloned()
    }
}

pub fn fit(w: f32, h: f32, max_w: f32, max_h: f32) -> (f32, f32) {
    let k = (max_w / w.max(1.0)).min(max_h / h.max(1.0)).min(1.0);
    ((w * k).max(24.0), (h * k).max(24.0))
}

/// What a reply's markdown in `s`'s open session needs to show its pictures.
pub fn md_media<S: TimelineHost>(s: &S) -> markdown::MdMedia {
    let store = s.media_store();
    markdown::MdMedia {
        store: store.markdown.clone(),
        directory: s.active_session_id().and_then(|id| s.session_directory(id)).map(std::rc::Rc::from),
        viewing: store.viewing.as_ref().map(|v| v.id.clone()),
        flight: viewer::flight(s),
        message: None,
        scope: None,
    }
}

/// `md` showing its images and link favicons from `m`, an image opening the viewer on a press
/// as Desktop's markdown images do (`image-preview.tsx`): a button named by its alt text that
/// flies into the viewer and back. Unreadable local images keep their inline alt text without
/// appending an error line below the reply (`markdown-image.ts:81-86`).
pub fn markdown_pictures<S: TimelineHost>(p: &Pal, md: rune_kit::Markdown, m: markdown::MdMedia) -> rune_kit::Markdown {
    let pal = *p;
    let (images, favicons) = (m.clone(), m.store.clone());
    let seen = RefCell::new(HashMap::<String, usize>::new());
    md.images(move |src| images.store.read_image(images.directory.as_deref(), src).ok().flatten())
        .link_favicon(move |url| favicons.favicon(url))
        .image_frame(move |img, f| {
            let place = format!("{}:{}:{}", m.scope.as_deref().or(m.message.as_deref()).unwrap_or(""), f.src, f.alt);
            let nth = {
                let mut seen = seen.borrow_mut();
                let n = seen.entry(place.clone()).or_insert(0);
                *n += 1;
                *n - 1
            };
            let id = format!("md-image:{place}:{nth}");
            if m.viewing.as_deref() == Some(id.as_str()) {
                return el().w_full().child(img.key(viewer::HELD).opacity(0.0));
            }
            let name = if f.alt.is_empty() { opencode_i18n::t!("ui.imagePreview.alt").to_owned() } else { f.alt.to_owned() };
            let fallback = f.image.clone();
            let dir = m.directory.as_deref().map(str::to_owned);
            let src = f.src.to_owned();
            let img = match m.store.natural_size(dir.as_deref(), &src) {
                Some([natural_w, _]) if natural_w > 0 => img.max_w(natural_w as f32),
                _ => img,
            };
            let dragged = Dragged::Markdown { directory: dir.clone(), src: src.clone() };
            let img = menu::on_image::<S>(draggable::<S>(img, dragged.clone()), dragged.clone(), m.message.as_deref());
            el().w_full().child(
                img.key("img")
                    .matched(id.clone())
                    .token_motion(m.flight)
                    .role("button")
                    .label(name.clone())
                    .focusable(true)
                    .focus_default(&pal)
                    .cursor(rune::Cursor::ZoomIn)
                    .on_press_tx(move |s: &mut S, tx: &mut rune::Tx| {
                        let md = &s.media_store().markdown;
                        let full = md.image_full(dir.as_deref(), &src).unwrap_or_else(|| fallback.clone());
                        let natural = md.natural_size(dir.as_deref(), &src);
                        viewer::open(
                            s,
                            Viewing {
                                id: id.clone(),
                                name: name.clone(),
                                resolution: viewer::Resolution::new(full, fallback.clone()),
                                natural,
                                dragged: dragged.clone(),
                                strip: None,
                            },
                            tx,
                        );
                    }),
            )
        })
}

/// Whether `gallery` shows anything for `files`, without loading them.
pub fn shows(files: &[Attachment]) -> bool {
    files.iter().any(|f| f.is_image() || f.unavailable().is_some())
}

/// Images in the transcript, right-aligned for a user: one at its own shape (at most 320×240
/// points, 2× pixels), several in a bento block (`bento`) as wide as the timeline's column less
/// `inset`, the room the caller indents it by. A ref still on its way holds a tile;
/// one whose bytes are gone shows the server's placeholder as a chip.
///
/// `key` names the gallery, and must be unique in the window: each image's shared-element id
/// is its place in it ([`gallery_id`]), which a refreshed page keeps.
///
/// `message`: the message the images belong to, whose items follow theirs in a right-click menu.
pub fn gallery<S: TimelineHost>(s: &S, p: &Pal, key: &str, files: &[Attachment], end: bool, inset: f32, message: Option<&str>) -> Option<El> {
    let images: Vec<&Attachment> = files.iter().filter(|f| shows(std::slice::from_ref(f))).collect();
    if images.is_empty() {
        return None;
    }
    let strip: Arc<[Attachment]> = images.iter().map(|&f| f.clone()).collect();
    let store = s.media_store();
    enum TileState { Ready(Image), Loading, Failed }
    // Pictures (ready, loading, or failed) go in the bento block; server-unavailable placeholders
    // show as named chips (`tool-result-images.tsx:78-94`, `message-part.css:821`).
    let mut shown = Vec::new();
    let mut chips = Vec::new();
    for (i, f) in images.into_iter().enumerate() {
        let k = format!("img-{i}");
        match store.image(f) {
            Some(img) if img.error().is_none() => shown.push((i, k, f, TileState::Ready(img))),
            None if store.fetched(f) == Some(Fetched::Loading) => shown.push((i, format!("{k}-loading"), f, TileState::Loading)),
            _ if f.is_image() && f.unavailable().is_none() => shown.push((i, format!("{k}-failed"), f, TileState::Failed)),
            _ => {
                let label = f.unavailable().map_or_else(|| f.name.clone(), str::to_owned);
                chips.push(row().key(k).h(32.0).px(10.0).center_y().radius(8.0).fill(p.layer2).child(text(label).font(SMALL).color(p.muted)));
            }
        }
    }
    let sizes: Vec<_> = shown
        .iter()
        .map(|(_, _, f, st)| if matches!(st, TileState::Ready(_)) { store.thumbnail_size(f) } else { (160.0, 120.0) })
        .collect();
    let ((bw, bh), tiles) = bento::tiles(&sizes, crate::genui::width(s) - inset);
    let mut block = el().key(key).size(bw, bh);
    for ((i, k, f, st), [x, y, w, h]) in shown.into_iter().zip(tiles) {
        let tile = el().key(k.clone()).label(f.name.clone()).size(w, h).radius(bento::RADIUS).fill(p.layer2);
        block = block.child(match st {
            TileState::Ready(img) => {
                let held = el().key(format!("{k}-held")).size(w, h).absolute(x, y);
                // The tile crops to its slot rather than letterboxing inside it.
                let thumb = tile.box_shadow(Pal::ring_in(p.border_muted)).pixels_fit(img.pixels(), SurfaceFit::Fill).absolute(x, y);
                // `[data-slot="tool-result-image"] { cursor: zoom-in }` (`message-part.css:772,799`).
                opens(s, thumb, held, Strip::new(key, strip.clone(), i), img, message).cursor(rune::Cursor::ZoomIn)
            }
            TileState::Loading => tile.box_shadow(Pal::ring_in(p.border_muted)).absolute(x, y),
            TileState::Failed => {
                let err_label = opencode_i18n::t!("ui.fileMedia.state.error", kind = opencode_i18n::t!("ui.fileMedia.kind.image"));
                let failed = tile
                    .box_shadow(Pal::ring_in(p.border_muted))
                    .center()
                    .absolute(x, y)
                    .child(rune::icon(&OUTLINE_PHOTO, 20.0).color(p.icon_faint));
                if f.media.is_some() {
                    let retry = f.clone();
                    // `[data-slot="tool-result-image"][data-retry] { cursor: pointer }` (`message-part.css:821`).
                    failed
                        .role("button")
                        .label(err_label)
                        .cursor(rune::Cursor::Pointer)
                        .on_press_tx(move |s: &mut S, tx: &mut rune::Tx| {
                            tx.instant();
                            s.media_store().retry(&retry);
                        })
                } else {
                    failed.role("img").label(err_label)
                }
            }
        });
    }
    let mut out = col().w_full().pb(8.0).gap(8.0);
    if bh > 0.0 {
        out = out.child(if end { row().w_full().justify_end().child(block) } else { block });
    }
    if !chips.is_empty() {
        let mut r = row().key(format!("{key}-chips")).w_full().gap(8.0).flex_wrap().children(chips);
        if end {
            r = r.justify_end();
        }
        out = out.child(r);
    }
    Some(out)
}

/// The shared-element id of the `index`th image in the gallery keyed `key`: its place, not its
/// bytes' address, which changes when a page is read again while the viewer shows it.
pub fn gallery_id(key: &str, index: usize) -> String {
    format!("{key}:{index}")
}

/// One attachment's bytes (or, for a ref, the attachment itself): a composer chip's id.
pub fn identity(f: &Attachment) -> String {
    match &f.media {
        Some(hash) => format!("{:x}:{hash}", std::ptr::from_ref(f) as usize),
        None => format!("{:x}:{}", f.bytes.as_ptr() as usize, f.bytes.len()),
    }
}
