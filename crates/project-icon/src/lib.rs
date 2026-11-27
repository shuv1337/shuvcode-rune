//! Project icons beside a project's name, for any Rune app on OpenCode: the server's image for
//! the project (Desktop's `icon.override` or `icon.url`, PNG, JPEG or SVG), else (unless
//! `set_repo_icons(false)`) the app's own icon found in its directory, else a quiet procedural
//! mark seeded by the project's name.
//!
//! ```ignore
//! let icons = project_icon::Icons::new(waker); // keep it in app state
//! row().child(icons.view(&palette, &project, 16.0)).child(text(&project.name))
//! ```
//!
//! Finding and decoding run on one worker thread, once per directory and source file's mtime,
//! and are checked again every two minutes. Repo icons are shrunk to 64 px in linear light with
//! premultiplied alpha, so 16 pt at 2× uses an exact mip; SVGs and marks are drawn at the size
//! they show at.

mod find;
mod finish;
mod funky;
mod retro;
mod sprites;
mod pixels;
mod styles;
mod unread;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use rune::task::Queue;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};
use rune::time::Instant;

use rune::{Color, El, Image, ImageCache, SurfaceFit, TextStyle, Waker, el, text};

use find::{Data, Key};
use pixels::Buf;
pub use finish::Finish;
pub use styles::{Family, Look, Style};

/// The theme colours icons are built from.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    /// Background and border of Desktop's avatar colours, in its order: orange, yellow, cyan,
    /// green, red, pink, blue, purple, gray (`avatar-bg-*`, `avatar-border-*`).
    pub avatars: [(Color, Color); 9],
    /// Letters and ink on an avatar colour (`avatar-fg`).
    pub fg: Color,
    /// The neutral tile behind a transparent logo.
    pub tile: Color,
    /// The hairline around an image.
    pub ring: Color,
}

/// What an icon is looked up by.
#[derive(Clone, Copy, Debug, Default)]
pub struct Project<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub directory: &'a str,
    /// The server's image for it, as Desktop picks it (`ProjectIcon::source`).
    pub image: Option<&'a str>,
    /// A chosen avatar colour (`orange`, …), or empty.
    pub color: &'a str,
}

impl<'a> Project<'a> {
    /// Just a name: no image is looked up, so it always gets a mark.
    pub fn named(name: &'a str) -> Project<'a> {
        Project { name, ..Project::default() }
    }
}

impl<'a> From<&'a opencode_kit::model::Project> for Project<'a> {
    fn from(p: &'a opencode_kit::model::Project) -> Project<'a> {
        Project { id: &p.id, name: &p.name, directory: &p.directory, image: p.icon.source(&p.id), color: &p.icon.color }
    }
}

/// How long a found icon (or its absence) stands before the directory is looked at again.
const RECHECK: Duration = Duration::from_secs(120);
/// Repo icons' pixels per side: 16 pt at 2× is the 32 px mip; the full level covers 32 pt.
const PX: u32 = 64;

type MarkKey = (String, bool, usize, u32, (Style, i32, i32), Finish);

#[derive(Default)]
pub struct Icons {
    waker: Option<Waker>,
    sync: bool,
    no_repo: bool,
    scale: Cell<f32>,
    look: Cell<Look>,
    finish: Cell<Finish>,
    inner: RefCell<Option<Inner>>,
    marks: RefCell<HashMap<MarkKey, Image>>,
    lights: RefCell<HashMap<(Finish, bool, u32, u32), Image>>,
    vectors: RefCell<HashMap<(u64, u32), Image>>,
    unread: RefCell<unread::Tiles>,
}

struct Inner {
    cache: Arc<ImageCache>,
    /// The worker that finds and decodes projects' icons, started by the first job (`Inner::jobs`).
    jobs: std::cell::OnceCell<Queue<Job>>,
    found: Arc<Mutex<HashMap<String, Found>>>,
    /// The worker changed what a tile shows since the app last asked (`Icons::take_found`).
    changed: Arc<AtomicBool>,
    wake: Waker,
}

impl Inner {
    /// The icon worker's queue: its thread starts with the first job, so a window that draws only
    /// letter tiles (repository icons off, no server images) starts none, and the first frame
    /// that draws a tile doesn't start a thread.
    fn jobs(&self) -> &Queue<Job> {
        self.jobs.get_or_init(|| {
            let (c, fd, changed, wake) = (self.cache.clone(), self.found.clone(), self.changed.clone(), self.wake.clone());
            rune::task::queue("project-icons", move |job: Job| {
                    // Once the tile's entry says what it shows, and only if that moved: the app's
                    // poll then redraws what reads the icons (the image cache's own wake can come
                    // before the entry is settled).
                    if settle(&fd, &job.entry, run(&c, &job)) {
                        changed.store(true, Ordering::Release);
                        wake();
                    }
                    std::ops::ControlFlow::Continue(())
                })
        })
    }
}

/// Cached source identity and decoded image (`icon` never exists without its `Key`).
type CachedIcon = (Key, Option<Shown>);

/// Lookup lifecycle for one project-icon cache entry (`found`).
#[derive(Default)]
enum Found {
    #[default]
    Unchecked,
    /// Worker lookup is in flight for `server`, keeping any prior `cached` icon visible.
    Checking { server: (usize, u64), cached: Option<CachedIcon> },
    /// Last lookup completed at `checked` for `server`.
    Settled { checked: Instant, server: (usize, u64), cached: Option<CachedIcon> },
}

enum FoundEvent {
    Check { stamp: (usize, u64), now: Instant },
    Settled { done: Option<(Key, Option<Shown>)>, now: Instant },
}

enum FoundEffect {
    None,
    Run { previous: Option<Key> },
    Settled { changed: bool },
}

impl Found {
    fn icon(&self) -> Option<&Shown> {
        match self {
            Self::Unchecked => None,
            Self::Checking { cached, .. } | Self::Settled { cached, .. } => {
                cached.as_ref().and_then(|(_, icon)| icon.as_ref())
            }
        }
    }

    fn step(&mut self, event: FoundEvent) -> FoundEffect {
        match (std::mem::take(self), event) {
            (Self::Unchecked, FoundEvent::Check { stamp, .. }) => {
                *self = Self::Checking { server: stamp, cached: None };
                FoundEffect::Run { previous: None }
            }
            (prev @ Self::Checking { .. }, FoundEvent::Check { .. }) => {
                *self = prev;
                FoundEffect::None
            }
            (Self::Settled { checked, server, cached }, FoundEvent::Check { stamp, now }) => {
                if now.saturating_duration_since(checked) > RECHECK || server != stamp {
                    let previous = cached.as_ref().map(|(k, _)| k.clone());
                    *self = Self::Checking { server: stamp, cached };
                    FoundEffect::Run { previous }
                } else {
                    *self = Self::Settled { checked, server, cached };
                    FoundEffect::None
                }
            }
            (prev, FoundEvent::Settled { done, now }) => {
                let (server, cached) = match prev {
                    Self::Unchecked => ((0, 0), None),
                    Self::Checking { server, cached } | Self::Settled { server, cached, .. } => (server, cached),
                };
                match done {
                    // The same source as before: keep its pixels.
                    Some((key, None)) if cached.as_ref().is_some_and(|(k, _)| *k == key) => {
                        *self = Self::Settled { checked: now, server, cached };
                        FoundEffect::Settled { changed: false }
                    }
                    Some((key, shown)) => {
                        let changed = shown.is_some() || cached.as_ref().is_some_and(|(_, i)| i.is_some());
                        *self = Self::Settled { checked: now, server, cached: Some((key, shown)) };
                        FoundEffect::Settled { changed }
                    }
                    None => {
                        let changed = cached.is_some_and(|(_, i)| i.is_some());
                        *self = Self::Settled { checked: now, server, cached: None };
                        FoundEffect::Settled { changed }
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
enum Shown {
    /// A full tile.
    Tile(Image, Arc<image::RgbaImage>),
    /// A transparent mark (a logo), drawn small on a neutral tile.
    Mark(Image, Arc<image::RgbaImage>),
    /// An SVG, rasterized at each size it shows at, over the avatar colour as Desktop draws it.
    Vector(Arc<[u8]>, u64, Arc<image::RgbaImage>),
}

struct Job {
    /// The entry in `found` it settles.
    entry: String,
    dir: String,
    name: String,
    server: Option<String>,
    repo: bool,
    previous: Option<Key>,
}

impl Icons {
    pub fn new(waker: Waker) -> Icons {
        let mut icons = Icons::default();
        icons.set_waker(waker);
        icons
    }

    /// Whether the worker changed what a tile shows (an image found, or gone) since the last call:
    /// what draws the icons must be built again. Call from the app's poll.
    pub fn take_found(&self) -> bool {
        self.inner.borrow().as_ref().is_some_and(|inner| inner.changed.swap(false, Ordering::AcqRel))
    }

    /// Call from `AppSpec::setup`, before the first view.
    pub fn set_waker(&mut self, waker: Waker) {
        self.waker = Some(waker);
    }

    /// Resolve and decode in the view, so a headless snapshot's first frame shows icons.
    pub fn set_sync(&mut self, sync: bool) {
        self.sync = sync;
    }

    /// Whether to look for an icon file in the project's directory when the server has none
    /// (default on). OpenCode Desktop never does: it shows the server's image or the letter.
    pub fn set_repo_icons(&mut self, on: bool) {
        self.no_repo = !on;
    }

    /// Device pixels per point that marks and SVGs rasterize for (default 2).
    pub fn set_scale(&self, scale: f32) {
        self.scale.set(scale);
    }

    /// The mark for projects without an image (default: Gradient).
    pub fn set_look(&self, look: Look) {
        self.look.set(look);
    }

    pub fn look(&self) -> Look {
        self.look.get()
    }

    /// How images and letter tiles catch the light (default: Flat, Desktop's own).
    pub fn set_finish(&self, finish: Finish) {
        self.finish.set(finish);
    }

    pub fn finish(&self) -> Finish {
        self.finish.get()
    }

    fn scale(&self) -> f32 {
        let s = self.scale.get();
        if s > 0.0 { s } else { 2.0 }
    }

    fn with<R>(&self, f: impl FnOnce(&Inner) -> R) -> R {
        let mut inner = self.inner.borrow_mut();
        let inner = inner.get_or_insert_with(|| {
            let wake = self.waker.clone().unwrap_or_else(|| Arc::new(|| {}));
            let cache = Arc::new(ImageCache::new(wake.clone()));
            Inner { cache, jobs: std::cell::OnceCell::new(), found: Default::default(), changed: Default::default(), wake }
        });
        f(inner)
    }

    /// The project's real icon once it's found and decoded.
    fn found(&self, p: &Project) -> Option<Shown> {
        let server = p.image.filter(|u| find::is_data_image(u));
        if server.is_none() && (p.directory.is_empty() || self.no_repo) {
            return None;
        }
        let stamp = server.map_or((0, 0), stamp);
        let dir = if p.directory.is_empty() { format!("id:{}", p.id) } else { p.directory.to_owned() };
        // Two servers can hold the same folder with different images: each image is its own entry.
        let entry = if server.is_some() { format!("{dir}\n{stamp:?}") } else { dir.clone() };
        self.with(|inner| {
            let job = {
                let mut found = inner.found.lock().unwrap_or_else(PoisonError::into_inner);
                let f = found.entry(entry.clone()).or_default();
                match f.step(FoundEvent::Check { stamp, now: Instant::now() }) {
                    FoundEffect::Run { previous } => Some(Job {
                        entry: entry.clone(),
                        dir,
                        name: p.name.to_owned(),
                        server: server.map(str::to_owned),
                        repo: !self.no_repo,
                        previous,
                    }),
                    FoundEffect::None | FoundEffect::Settled { .. } => None,
                }
            };
            if let Some(job) = job {
                if self.sync {
                    let done = run(&inner.cache, &job);
                    if let Some((_, Some(Shown::Tile(i, _) | Shown::Mark(i, _)))) = &done {
                        i.wait().ok();
                    }
                    settle(&inner.found, &entry, done);
                } else {
                    inner.jobs().send(job).ok();
                }
            }
            let found = inner.found.lock().unwrap_or_else(PoisonError::into_inner);
            found.get(&entry)?.icon().cloned().filter(|s| match s {
                Shown::Tile(i, _) | Shown::Mark(i, _) => i.is_ready() && i.error().is_none(),
                Shown::Vector(..) => true,
            })
        })
    }

    fn vector(&self, bytes: &Arc<[u8]>, hash: u64, size_pt: f32) -> Image {
        let px = (size_pt * self.scale()).ceil() as u32;
        if let Some(img) = self.vectors.borrow().get(&(hash, px)) {
            return img.clone();
        }
        let img = self.with(|inner| inner.cache.svg(bytes.clone(), size_pt, self.scale()));
        if self.sync {
            img.wait().ok();
        }
        self.vectors.borrow_mut().insert((hash, px), img.clone());
        img
    }

    /// The project's icon at `size_pt` square: its image, else a mark in the current look.
    pub fn view<'a>(&self, pal: &Palette, project: impl Into<Project<'a>>, size_pt: f32) -> El {
        self.view_with_label(pal, project, size_pt, None, false)
    }

    /// [`Self::view`] with an optional stacked shortcut label (`project-avatar.tsx` `label`,
    /// `tab-nav.css` #63089): when `show_label` is true, the initial slides up and fades out
    /// while `label` rises in over `160 ms ease-out` (backing images with the avatar colour).
    pub fn view_with_label<'a>(
        &self,
        pal: &Palette,
        project: impl Into<Project<'a>>,
        size_pt: f32,
        label: Option<&str>,
        show_label: bool,
    ) -> El {
        let project = project.into();
        let Some(lbl) = label else {
            return self.image(pal, project, size_pt).unwrap_or_else(|| self.mark(pal, project, size_pt, self.look()));
        };
        let anim = rune::Anim::new(rune::Curve::tween(0.16, rune::Easing::EASE_OUT));
        let radius = size_pt * styles::RADIUS;
        let font = TextStyle::new(size_pt * 11.0 / 16.0, 530.0).lh(size_pt).tracking(0.05).features("\"tnum\"");
        let badge_label = |bg: Option<Color>| {
            let mut layer = el()
                .key("avatar-label")
                .absolute(0.0, 0.0)
                .size(size_pt, size_pt)
                .center()
                .opacity(if show_label { 1.0 } else { 0.0 })
                .offset(0.0, if show_label { 0.0 } else { 6.0 })
                .opacity_motion(anim)
                .offset_motion(anim)
                .a11y_hidden()
                .child(text(lbl).font(font).color(pal.fg));
            if let Some(bg) = bg {
                layer = layer.fill(bg);
            }
            layer
        };
        if let Some(img_tile) = self.image(pal, project, size_pt) {
            let bg = pal.avatars[chosen_color(project.color).unwrap_or(8)].0;
            return el()
                .size(size_pt, size_pt)
                .radius(radius)
                .clip_soft(0.0)
                .child(img_tile)
                .child(badge_label(Some(bg)));
        }
        self.mark_with_label(pal, project, size_pt, self.look(), Some((lbl, show_label, anim, font)))
    }

    /// What lies over a tile in the current finish: Desktop's inset ring in `ring`, or the light,
    /// which draws its own edge.
    fn top(&self, size_pt: f32, image: bool, ring_color: Color) -> El {
        let finish = self.finish();
        let radius = size_pt * styles::RADIUS;
        let mut top = el().key("ring").absolute(0.0, 0.0).size(size_pt, size_pt).radius(radius);
        if finish.ringed() && ring_color.a > 0.0 {
            top = top.box_shadow(ring(ring_color));
        }
        if !finish.lit() {
            return top;
        }
        let px = (size_pt * self.scale()).ceil().clamp(1.0, 1024.0) as u32;
        let key = (finish, image, px, (size_pt * 4.0).round() as u32);
        let cached = self.lights.borrow().get(&key).cloned();
        let img = cached.unwrap_or_else(|| {
            let img = self.with(|inner| inner.cache.rgba(px, px, finish::light(finish, image, size_pt, px)));
            if self.sync {
                img.wait().ok();
            }
            self.lights.borrow_mut().insert(key, img.clone());
            img
        });
        let light = el().key("light").absolute(0.0, 0.0).size(size_pt, size_pt).radius(radius).pixels_fit(img.pixels(), SurfaceFit::Fill);
        top.child(light)
    }

    /// A finished tile: lit finishes cast a shadow.
    fn lift(&self, pal: &Palette, tile: El, size_pt: f32) -> El {
        let finish = self.finish();
        if finish.lit() { finish::shadows(finish, tile, size_pt, pal.dark) } else { tile }
    }

    /// The project's real icon at `size_pt` square, or None while there's none (no mark).
    pub fn image<'a>(&self, pal: &Palette, project: impl Into<Project<'a>>, size_pt: f32) -> Option<El> {
        let project = project.into();
        let radius = size_pt * styles::RADIUS;
        let variant = chosen_color(project.color);
        Some(match self.found(&project)? {
            Shown::Tile(img, _) => self.picture(pal, &img, variant, size_pt),
            Shown::Mark(img, _) => {
                let inner = el().size(size_pt * 0.75, size_pt * 0.75).pixels_fit(img.pixels(), SurfaceFit::Fit);
                let tile = el().size(size_pt, size_pt).radius(radius).fill(pal.tile).center().child(inner);
                self.lift(pal, tile.child(self.top(size_pt, true, pal.ring)), size_pt)
            }
            Shown::Vector(bytes, hash, _) => self.picture(pal, &self.vector(&bytes, hash, size_pt), variant, size_pt),
        })
    }

    /// An image on Desktop's avatar surface (`project-avatar.css`): the avatar colour, its inset
    /// ring, and the `<img>` over both, so an opaque image hides the ring. A lit finish's light
    /// lies over the image.
    fn picture(&self, pal: &Palette, img: &Image, variant: Option<usize>, size_pt: f32) -> El {
        let radius = size_pt * styles::RADIUS;
        let (bg, border) = pal.avatars[variant.unwrap_or(8)];
        let pic = el().key("image").absolute(0.0, 0.0).size(size_pt, size_pt).radius(radius).pixels_fit(img.pixels(), SurfaceFit::Fill);
        let top = self.top(size_pt, true, border);
        let tile = el().size(size_pt, size_pt).radius(radius).fill(bg);
        let tile = if self.finish().lit() { tile.child(pic).child(top) } else { tile.child(top).child(pic) };
        self.lift(pal, tile, size_pt)
    }

    /// The procedural mark alone, in `look`.
    pub fn mark<'a>(&self, pal: &Palette, project: impl Into<Project<'a>>, size_pt: f32, look: Look) -> El {
        self.mark_with_label(pal, project, size_pt, look, None)
    }

    fn mark_with_label<'a>(
        &self,
        pal: &Palette,
        project: impl Into<Project<'a>>,
        size_pt: f32,
        look: Look,
        overlay: Option<(&str, bool, rune::Anim, TextStyle)>,
    ) -> El {
        let project = project.into();
        let seed = if project.name.is_empty() { project.id } else { project.name };
        let chosen = chosen_color(project.color);
        // Desktop shows gray unless a colour is chosen; the others pick one from the name.
        let finish = self.finish();
        let variant = match look.style {
            Style::Desktop => chosen.unwrap_or(8),
            _ => chosen.unwrap_or((styles::hash(seed) % 8) as usize),
        };
        // Desktop's letter tile under a light that draws its own edge gets a ringless body.
        let lit_body = look.style == Style::Desktop && !finish.ringed();
        let px = (size_pt * self.scale()).ceil().clamp(1.0, 1024.0) as u32;
        // Desktop's tile is its colour, size and finish alone (the letter is text over it): every
        // project of a colour shares one image, so a list of projects draws one tile, not one each.
        let named = if look.style == Style::Desktop { String::new() } else { seed.to_owned() };
        let key = (named, pal.dark, variant, px, look.key(), if look.style == Style::Desktop { finish } else { Finish::Flat });
        let cached = self.marks.borrow().get(&key).cloned();
        let img = cached.unwrap_or_else(|| {
            let rgba = if lit_body { finish::body(pal, variant, finish, px) } else { styles::draw(pal, seed, variant, &look, size_pt, px) };
            let img = self.with(|inner| inner.cache.rgba(px, px, rgba));
            if self.sync {
                img.wait().ok();
            }
            self.marks.borrow_mut().insert(key, img.clone());
            img
        });
        let radius = size_pt * styles::RADIUS;
        let lifted = look.style == Style::Desktop && finish.lit();
        let mut tile = if img.is_ready() {
            el().size(size_pt, size_pt).radius(radius).pixels_fit(img.pixels(), SurfaceFit::Fill)
        } else {
            let (bg, border) = pal.avatars[variant];
            let tile = el().size(size_pt, size_pt).radius(radius).fill(bg);
            // A lifted tile keeps the drop shadows it has once its image is ready, in their places,
            // so they don't morph from the placeholder's ring when it arrives.
            if lifted { tile } else { tile.box_shadow(ring(border)) }
        };
        if look.style.lettered() {
            // Desktop: the first character of the name (an unnamed project's folder), upper-cased;
            // the others skip punctuation.
            let first = if look.style == Style::Desktop {
                let folder = opencode_kit::path::basename(project.directory);
                (if project.name.is_empty() { folder } else { project.name }).chars().next().or_else(|| seed.chars().next())
            } else {
                seed.chars().find(|c| c.is_alphanumeric())
            };
            let letter = first.map_or("?".into(), |c| c.to_uppercase().to_string());
            // Desktop's 11 px semibold letter in a 16 px tile.
            let font = TextStyle::new(size_pt * 11.0 / 16.0, 530.0).lh(size_pt);
            if let Some((lbl, show_label, anim, lbl_font)) = overlay {
                let initial = text(letter)
                    .key("avatar-initial")
                    .font(font)
                    .color(pal.fg)
                    .opacity(if show_label { 0.0 } else { 1.0 })
                    .offset(0.0, if show_label { -6.0 } else { 0.0 })
                    .opacity_motion(anim)
                    .offset_motion(anim);
                let shortcut = el()
                    .key("avatar-label")
                    .absolute(0.0, 0.0)
                    .size(size_pt, size_pt)
                    .center()
                    .opacity(if show_label { 1.0 } else { 0.0 })
                    .offset(0.0, if show_label { 0.0 } else { 6.0 })
                    .opacity_motion(anim)
                    .offset_motion(anim)
                    .a11y_hidden()
                    .child(text(lbl).font(lbl_font).color(pal.fg));
                tile = tile.clip_soft(0.0).center().child(initial).child(shortcut);
            } else {
                tile = tile.center().child(text(letter).font(font).color(pal.fg));
            }
        } else if let Some((lbl, show_label, anim, lbl_font)) = overlay {
            let bg = pal.avatars[variant].0;
            let shortcut = el()
                .key("avatar-label")
                .absolute(0.0, 0.0)
                .size(size_pt, size_pt)
                .fill(bg)
                .center()
                .opacity(if show_label { 1.0 } else { 0.0 })
                .offset(0.0, if show_label { 0.0 } else { 6.0 })
                .opacity_motion(anim)
                .offset_motion(anim)
                .a11y_hidden()
                .child(text(lbl).font(lbl_font).color(pal.fg));
            tile = tile.clip_soft(0.0).child(shortcut);
        }
        if look.style == Style::Pop {
            let letter = seed.chars().find(|c| c.is_alphanumeric()).map_or("?".into(), |c| c.to_uppercase().to_string());
            let font = TextStyle::new(size_pt * 0.74, 860.0).lh(size_pt);
            let (ink, shadow) = funky::pop_ink();
            let off = (size_pt * 0.07).max(1.0);
            let layer = |dx: f32, color: Color| {
                el().absolute(dx, dx).size(size_pt, size_pt).center().child(text(letter.clone()).font(font).color(color))
            };
            tile = tile.child(layer(off * 0.5, shadow)).child(layer(-off * 0.5, ink));
        }
        if lifted {
            // The light goes over the letter, as iOS's does.
            tile = tile.child(self.top(size_pt, false, Color::TRANSPARENT));
            return self.lift(pal, tile, size_pt);
        }
        tile
    }
}

/// Records what the lookup found for `dir`; true when what its tile shows changed.
fn settle(found: &Mutex<HashMap<String, Found>>, dir: &str, done: Option<(Key, Option<Shown>)>) -> bool {
    let mut found = found.lock().unwrap_or_else(PoisonError::into_inner);
    let f = found.entry(dir.to_owned()).or_default();
    matches!(f.step(FoundEvent::Settled { done, now: Instant::now() }), FoundEffect::Settled { changed: true })
}

/// Desktop's `box-shadow: inset 0 0 0 0.5px` (`project-avatar.css`): a ring just inside the tile,
/// at its fractional width.
fn ring(c: Color) -> rune::Shadow {
    rune::Shadow { spread: 0.5, color: c, inset: true, ..rune::Shadow::default() }
}

/// Desktop's avatar colour names, as indices into `Palette::avatars`.
fn chosen_color(name: &str) -> Option<usize> {
    let name = match name {
        "mint" => "cyan",
        "lime" => "green",
        n => n,
    };
    ["orange", "yellow", "cyan", "green", "red", "pink", "blue", "purple", "gray"].iter().position(|n| *n == name)
}

fn stamp(url: &str) -> (usize, u64) {
    let b = url.as_bytes();
    let ends = [&b[..b.len().min(256)], &b[b.len().saturating_sub(256)..]].concat();
    (b.len(), styles::fnv(&ends))
}

/// Resolves a job: its source and, when that changed, the decoded icon. None when there is no
/// icon at all.
fn run(cache: &ImageCache, job: &Job) -> Option<(Key, Option<Shown>)> {
    if let Some(url) = &job.server {
        let key = Key(format!("server:{:?}", stamp(url)), SystemTime::UNIX_EPOCH);
        if job.previous.as_ref() == Some(&key) {
            return Some((key, None));
        }
        match find::data_url(url) {
            Some(Data::Svg(bytes)) => {
                let hash = styles::fnv(&bytes);
                let miniature = unread::svg_miniature(&bytes)?;
                return Some((key, Some(Shown::Vector(bytes.into(), hash, Arc::new(miniature)))));
            }
            Some(Data::Raster(bytes)) => {
                // Desktop's `<img>` fills the tile as it is (`object-fit: cover`), never trimmed or
                // set small on a plate as a found repo icon is. Pre-crop/downscale before `Buf`
                // (16 B/px f32) so a 4K server icon never spikes hundreds of megabytes for a 64 px tile.
                if let Ok(mut reader) = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format() {
                    let mut limits = image::Limits::default();
                    limits.max_image_width = Some(4096);
                    limits.max_image_height = Some(4096);
                    limits.max_alloc = Some(64 * 1024 * 1024);
                    reader.limits(limits);
                    if let Ok(icon) = reader.decode() {
                        let rgba = if icon.width().max(icon.height()) > PX * 4 {
                            icon.resize_to_fill(PX * 2, PX * 2, image::imageops::FilterType::Triangle).into_rgba8()
                        } else {
                            icon.into_rgba8()
                        };
                        let buf = Buf::from_rgba8(&rgba).cover();
                        return Some((key, Some(raster(cache, buf, false))));
                    }
                }
            }
            None => {}
        }
    }
    if !job.repo || job.dir.is_empty() || job.dir.starts_with("id:") {
        return None;
    }
    let mut cands = find::candidates(Path::new(&job.dir), &job.name);
    cands.sort_by_key(|c| c.rank());
    for c in cands {
        let Some(key) = c.key() else { continue };
        if job.previous.as_ref() == Some(&key) {
            return Some((key, None));
        }
        if let Some((buf, trim)) = find::decode(&c) {
            return Some((key, Some(finish(cache, buf, trim))));
        }
    }
    None
}

fn finish(cache: &ImageCache, buf: Buf, trim: bool) -> Shown {
    let buf = if trim { buf.trim() } else { buf };
    let mark = buf.coverage() < 0.6;
    raster(cache, buf, mark)
}

fn raster(cache: &ImageCache, buf: Buf, mark: bool) -> Shown {
    let rgba = buf.resize(PX, PX).to_rgba8();
    let image = cache.rgba(PX, PX, rgba.as_slice());
    let src = Arc::new(image::RgbaImage::from_raw(PX, PX, rgba).unwrap_or_else(|| image::RgbaImage::new(PX, PX)));
    if mark { Shown::Mark(image, src) } else { Shown::Tile(image, src) }
}
