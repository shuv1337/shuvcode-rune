//! Images plugins draw: the ones a plugin hands over or loads (`Images`, `ui.image`). Each is
//! decoded by the window's Rune image cache on its own threads; a loaded one's bytes are read
//! first on the store's worker. Every image runs the [`load::Load`] machine; the host's poll
//! steps it, so a render only reads it.

pub mod load;

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use rune::{Image, ImageCache};

use load::{Load, LoadEffect, LoadEvent, Source};

/// Maximum concurrent plugin image fetch+decode tasks.
const MAX_READERS: usize = 4;
/// Maximum source-keyed plugin images kept across rest (favicons, web/data URLs).
const MAX_SOURCE_ENTRIES: usize = 128;
/// Maximum decoded pixel bytes kept for source-keyed plugin images across rest.
const MAX_SOURCE_PIXELS: usize = 32 << 20;

type ReadPool = Arc<Mutex<(VecDeque<(u32, Source)>, usize)>>;

/// Explicit lifecycle state of the plugin image store.
#[derive(Default)]
enum StoreState {
    #[default]
    Unstarted,
    Started {
        cache: Arc<ImageCache>,
        waker: rune::Waker,
        pool: ReadPool,
    },
}

#[derive(Default)]
pub struct Images {
    state: StoreState,
    tick: Cell<u64>,
    /// Id → the plugin it belongs to and where it stands.
    map: HashMap<u32, Entry>,
    /// A plugin's loads by source key, so the same source is the same image.
    by_source: HashMap<(String, String), u32>,
    next: u32,
    /// What the readers read, already decoding, waiting for the poll.
    fetched: Arc<Mutex<Vec<Fetched>>>,
    /// The videos plugins show (`ui.video`), by plugin and URL: one player each.
    players: HashMap<(String, String), rune::Player>,
    /// Players seen failed: a new failure redraws the surfaces that show it (`video_failed`).
    failed: HashSet<(String, String)>,
}

/// An image's id and what a reader read for it: its picture, decoding, and its bytes.
type Fetched = (u32, Result<(Image, Arc<[u8]>), String>);

struct Entry {
    plugin: String,
    load: Load,
    source_key: Option<String>,
    last_used: Cell<u64>,
}

impl Images {
    fn next_tick(&self) -> u64 {
        let t = self.tick.get().wrapping_add(1);
        self.tick.set(t);
        t
    }

    fn cache(&self) -> Option<&Arc<ImageCache>> {
        match &self.state {
            StoreState::Started { cache, .. } => Some(cache),
            StoreState::Unstarted => None,
        }
    }

    /// Starts the cache once the window's waker exists; a decoded image wakes it.
    pub fn start(&mut self, waker: rune::Waker) {
        if matches!(self.state, StoreState::Unstarted) {
            self.state = StoreState::Started {
                cache: Arc::new(ImageCache::new(waker.clone())),
                waker,
                pool: Arc::new(Mutex::new((VecDeque::new(), 0))),
            };
        }
    }

    /// Plugin `plugin`'s image from `bytes`: encoded when `width` is 0, else RGBA8. 0 when refused.
    pub fn add(&mut self, plugin: &str, bytes: &[u8], width: u32, height: u32) -> u32 {
        let Some(cache) = self.cache().cloned() else { return 0 };
        if bytes.is_empty() || (width > 0 && bytes.len() != width as usize * height as usize * 4) {
            return 0;
        }
        let bytes: Arc<[u8]> = bytes.into();
        let image = if width == 0 { cache.load(bytes.clone()) } else { cache.rgba(width, height, bytes.clone()) };
        self.insert(plugin, Load::Decoding { image, bytes }, None)
    }

    /// Plugin `plugin`'s SVG image, rasterized at twice its own size.
    pub fn add_svg(&mut self, plugin: &str, svg: &str) -> u32 {
        let Some(cache) = self.cache().cloned() else { return 0 };
        let bytes: Arc<[u8]> = svg.as_bytes().into();
        let image = cache.load(bytes.clone());
        self.insert(plugin, Load::Decoding { image, bytes }, None)
    }

    /// Plugin `plugin`'s image from `source`, known by `key`: the image it loaded from there
    /// before (loading, loaded or failed), else a new one its bounded worker pool starts reading.
    pub fn load(&mut self, plugin: &str, key: String, source: impl FnOnce() -> Option<Source>) -> u32 {
        if let Some(&id) = self.by_source.get(&(plugin.to_owned(), key.clone())) {
            let now = self.next_tick();
            if let Some(entry) = self.map.get(&id) {
                entry.last_used.set(now);
            }
            return id;
        }
        let id = self.insert(plugin, Load::Fetching, Some(key.clone()));
        self.by_source.insert((plugin.to_owned(), key), id);
        let sent = match source() {
            Some(source) => {
                if self.enqueue(id, source) {
                    Ok(())
                } else {
                    Err("this window loads no images")
                }
            }
            None => Err("no such image source"),
        };
        if let Err(why) = sent {
            self.step(id, LoadEvent::Fetched(Err(why.into())));
        }
        self.trim_sources(MAX_SOURCE_ENTRIES * 2, MAX_SOURCE_PIXELS * 2, Some(id));
        id
    }

    fn insert(&mut self, plugin: &str, load: Load, source_key: Option<String>) -> u32 {
        let now = self.next_tick();
        self.next += 1;
        self.map.insert(self.next, Entry { plugin: plugin.to_owned(), load, source_key, last_used: Cell::new(now) });
        self.next
    }

    fn enqueue(&mut self, id: u32, source: Source) -> bool {
        struct ActiveGuard { pool: ReadPool, armed: bool }
        impl Drop for ActiveGuard {
            fn drop(&mut self) {
                if self.armed {
                    let mut g = self.pool.lock().unwrap_or_else(|e| e.into_inner());
                    g.1 = g.1.saturating_sub(1);
                }
            }
        }
        let StoreState::Started { cache, waker, pool } = &self.state else { return false };
        let (cache, waker, pool) = (cache.clone(), waker.clone(), pool.clone());
        let mut guard = pool.lock().unwrap_or_else(|e| e.into_inner());
        guard.0.push_back((id, source));
        if guard.1 >= MAX_READERS {
            return true;
        }
        guard.1 += 1;
        drop(guard);
        let fetched = self.fetched.clone();
        let pool_w = pool.clone();
        if rune::task::spawn("plugin-images-read", move || {
            let mut active = ActiveGuard { pool: pool_w.clone(), armed: true };
            loop {
                let next = {
                    let mut g = pool_w.lock().unwrap_or_else(|e| e.into_inner());
                    match g.0.pop_front() {
                        Some(item) => item,
                        None => {
                            g.1 = g.1.saturating_sub(1);
                            active.armed = false;
                            return;
                        }
                    }
                };
                let (id, source) = next;
                let read = source.read().map(|bytes| {
                    let bytes: Arc<[u8]> = bytes.into();
                    (cache.load(bytes.clone()), bytes)
                });
                fetched.lock().unwrap_or_else(|e| e.into_inner()).push((id, read));
                waker();
            }
        })
        .is_ok()
        {
            return true;
        }
        let mut g = pool.lock().unwrap_or_else(|e| e.into_inner());
        g.1 = g.1.saturating_sub(1);
        if g.1 == 0 {
            if let Some(pos) = g.0.iter().rposition(|(queued, _)| *queued == id) {
                g.0.remove(pos);
            }
            return false;
        }
        true
    }

    /// Evicts oldest source-keyed plugin images (favicons, web/data URLs) above rest limits.
    pub fn rest(&mut self) {
        self.trim_sources(MAX_SOURCE_ENTRIES, MAX_SOURCE_PIXELS, None);
    }

    fn trim_sources(&mut self, max_entries: usize, max_pixels: usize, pin: Option<u32>) {
        let weight_of = |e: &Entry| match &e.load {
            Load::Loaded { size: [w, h], .. } => (*w as usize).saturating_mul(*h as usize).saturating_mul(16) / 3,
            Load::Decoding { bytes, .. } => bytes.len(),
            _ => 0,
        };
        let mut count = self.by_source.len();
        let mut pixels: usize = self
            .map
            .values()
            .filter(|e| e.source_key.is_some())
            .map(&weight_of)
            .sum();
        while count > 1 && (count > max_entries || pixels > max_pixels) {
            let need_pixels = count <= max_entries;
            let oldest = self
                .map
                .iter()
                .filter(|(id, e)| {
                    e.source_key.is_some()
                        && pin != Some(**id)
                        && !matches!(e.load, Load::Fetching)
                        && (!need_pixels || weight_of(e) > 0)
                })
                .min_by_key(|(_, e)| e.last_used.get())
                .map(|(&id, _)| id);
            let Some(id) = oldest else { break };
            if let Some(removed) = self.map.remove(&id) {
                pixels = pixels.saturating_sub(weight_of(&removed));
                if let Some(key) = removed.source_key {
                    self.by_source.remove(&(removed.plugin, key));
                    count = count.saturating_sub(1);
                }
            }
        }
    }

    /// Steps every image whose bytes landed or whose decode settled; true when what a render
    /// reads moved. At rest it does nothing.
    pub fn poll(&mut self) -> bool {
        let fetched = std::mem::take(&mut *self.fetched.lock().unwrap_or_else(|e| e.into_inner()));
        let mut changed = false;
        for (id, read) in fetched {
            changed |= self.step(id, LoadEvent::Fetched(read));
        }
        let settled: Vec<u32> = self
            .map
            .iter()
            .filter(|(_, e)| matches!(&e.load, Load::Decoding { image, .. } if image.is_ready()))
            .map(|(id, _)| *id)
            .collect();
        for id in settled {
            changed |= self.step(id, LoadEvent::Decoded);
        }
        for (key, player) in &self.players {
            if matches!(player.state(), rune::PlayerState::Failed(_)) && !self.failed.contains(key) {
                self.failed.insert(key.clone());
                changed = true;
            }
        }
        changed
    }

    fn step(&mut self, id: u32, event: LoadEvent) -> bool {
        let Some(entry) = self.map.get_mut(&id) else { return false };
        let (next, effect) = std::mem::replace(&mut entry.load, Load::Fetching).step(event);
        entry.load = next;
        effect == Some(LoadEffect::Changed)
    }

    /// Plugin `plugin`'s image `id`, if it has one by that id.
    pub fn get(&self, plugin: &str, id: u32) -> Option<&Load> {
        let entry = self.map.get(&id).filter(|e| e.plugin == plugin)?;
        entry.last_used.set(self.next_tick());
        Some(&entry.load)
    }

    pub fn remove(&mut self, plugin: &str, id: u32) {
        if self.map.get(&id).is_some_and(|e| e.plugin == plugin) {
            self.map.remove(&id);
            self.by_source.retain(|_, v| *v != id);
        }
    }

    /// Forgets everything plugin `plugin` handed over, loaded or played (it stopped).
    pub fn forget(&mut self, plugin: &str) {
        self.map.retain(|_, e| e.plugin != plugin);
        self.by_source.retain(|(p, _), _| p != plugin);
        self.players.retain(|(p, _), _| p != plugin);
        self.failed.retain(|(p, _)| p != plugin);
    }

    /// Plugin `plugin`'s player of video `url`, made playing muted on a loop (`autoplay loop
    /// muted`) the first time it shows; it plays only while an element draws it.
    pub fn player(&mut self, plugin: &str, url: &str) -> rune::Player {
        self.players
            .entry((plugin.to_owned(), url.to_owned()))
            .or_insert_with(|| {
                let player = rune::Player::url(url);
                // Its state changes wake the window, so a clip that fails is redrawn as one
                // (`video_failed`: the box shows what's behind it, not Rune's placeholder).
                if let StoreState::Started { waker, .. } = &self.state {
                    player.set_waker(waker.clone());
                }
                player.set_muted(true);
                player.set_looping(true);
                player.play();
                player
            })
            .clone()
    }

    /// Whether plugin `plugin`'s video at `url` can't play.
    pub fn video_failed(&self, plugin: &str, url: &str) -> bool {
        self.players
            .get(&(plugin.to_owned(), url.to_owned()))
            .is_some_and(|p| matches!(p.state(), rune::PlayerState::Failed(_)))
    }
}

/// An image handle for the viewer, with the bytes its menu saves.
pub fn shown(load: &Load) -> Option<(Image, Arc<[u8]>)> {
    Some((load.image()?.clone(), load.bytes()?.clone()))
}
