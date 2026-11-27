use std::sync::Arc;

use rune::{Cx, El, Image, Key, KeyEvent, Spring, SpringPair, SurfaceFit, Transition, Tx, el};
use rune_kit::{ImageZoom, Preview};

use opencode_kit::model::Attachment;

use super::{Dragged, draggable};
use crate::TimelineHost;
use crate::palette::Pal;

// The viewer: Desktop's image preview (`@opencode/ui` `image-preview.tsx`). A click on any
// timeline image (an attachment, a tool's picture, a reply's markdown image) grows it out of its
// place into the image, fitted to the window over a scrim with the zoom bar under it
// (rune-kit's `Preview`: zoom, pan, keys); Escape, the close button or a click on the backdrop
// shrinks it back into its thumbnail.

/// Explicit promotion state from the already-ready thumbnail to the on-demand full-res image.
pub(crate) enum Resolution {
    Decoding { full: Image, fallback: Image },
    Ready(Image),
}

impl Resolution {
    pub(crate) fn new(full: Image, fallback: Image) -> Self {
        if full.is_ready() {
            if full.error().is_none() { Self::Ready(full) } else { Self::Ready(fallback) }
        } else {
            Self::Decoding { full, fallback }
        }
    }

    /// Advances `Decoding` -> `Ready` once `full` settles. True when the displayed image changed.
    fn poll(&mut self) -> bool {
        let Self::Decoding { full, fallback } = self else { return false };
        if !full.is_ready() {
            return false;
        }
        let next = if full.error().is_none() { full.clone() } else { fallback.clone() };
        *self = Self::Ready(next);
        true
    }

    fn shown(&self) -> &Image {
        match self {
            Self::Decoding { fallback, .. } => fallback,
            Self::Ready(img) => img,
        }
    }
}

pub struct Viewing {
    /// The thumbnail's `El::matched` id.
    pub(crate) id: String,
    pub(crate) name: String,
    /// Full-resolution image promotion state (shows thumbnail until full decode lands).
    pub(crate) resolution: Resolution,
    /// Natural pixel dimensions from the header so fit/zoom math is exact before full decode lands.
    pub(crate) natural: Option<[u32; 2]>,
    /// What the open image's right-click menu acts on.
    pub(crate) dragged: Dragged,
    /// The gallery it was opened from, which ← and → step through.
    pub(crate) strip: Option<Strip>,
}

/// A gallery's files, keyed as [`super::gallery`] keys them, and which of them is open.
#[derive(Clone)]
pub struct Strip {
    pub(crate) key: String,
    pub(crate) files: Arc<[Attachment]>,
    pub(crate) at: usize,
}

impl Strip {
    pub fn new(key: impl Into<String>, files: Arc<[Attachment]>, at: usize) -> Strip {
        Strip { key: key.into(), files, at }
    }

    /// The open image's shared-element id.
    fn id(&self) -> String {
        super::gallery_id(&self.key, self.at)
    }
}

/// The key of the empty box that holds the viewed image's place: one shows at a time.
pub(crate) const HELD: &str = "image-viewer-held";

/// Desktop flies on a 0.3 s spring launched with a push, as a spring from rest starts slowly;
/// from rest, 0.25 s lands on the same shape (the Timeline knob's default).
pub(crate) fn flight<S: TimelineHost>(s: &S) -> Spring {
    Spring::new(s.knobs().get(&crate::knobs::IMAGE_FLIGHT), 0.0)
}

/// Opens the viewer on `v`, fitted, with focus on its close button (`image-preview.tsx:710-714`).
pub(crate) fn open<S: TimelineHost>(s: &mut S, v: Viewing, tx: &mut Tx) {
    show(s, v);
    tx.focus(rune_kit::image_zoom::CLOSE_KEY, false);
}

/// Shows `v` in the viewer, fitted.
fn show<S: TimelineHost>(s: &mut S, v: Viewing) {
    let store = s.media_store_mut();
    store.zoom.reset();
    store.viewing = Some(v);
}

/// A thumbnail that opens the viewer; while the viewer shows it, an empty box of its size (keyed
/// apart, so the thumbnail leaves the tree) holds its place, and the image flies out of it and
/// back into it. Dragged past a few points, it goes out as its image's file instead.
///
/// `strip`: the gallery it belongs to, which the viewer steps through with ← and → (a lone
/// image is a strip of one).
pub fn opens<S: TimelineHost>(s: &S, thumb: El, held: El, strip: Strip, image: Image, message: Option<&str>) -> El {
    let id = strip.id();
    if s.media_store().viewing.as_ref().is_some_and(|v| v.id == id) {
        return held.key(HELD);
    }
    let f = &strip.files[strip.at];
    let (name, dragged) = (f.name.clone(), Dragged::File(f.clone()));
    let thumb = super::menu::on_image::<S>(draggable::<S>(thumb, dragged.clone()), dragged.clone(), message);
    thumb.matched(id.clone()).token_motion(flight(s)).on_press_tx(move |s: &mut S, tx: &mut Tx| {
        let store = s.media_store();
        let f = &strip.files[strip.at];
        let full = store.image_full(f).unwrap_or_else(|| image.clone());
        let natural = store.natural_size(f);
        open(
            s,
            Viewing {
                id: id.clone(),
                name: name.clone(),
                resolution: Resolution::new(full, image.clone()),
                natural,
                dragged: dragged.clone(),
                strip: Some(strip.clone()),
            },
            tx,
        );
    })
}

/// Opens the viewer on the `index`th of `files`, the gallery keyed `key`, as pressing its
/// thumbnail does (the storybook's images stories). False while its image isn't loaded.
pub fn view_image<S: TimelineHost>(s: &mut S, key: &str, files: &[Attachment], index: usize, tx: &mut Tx) -> bool {
    let Some(v) = viewing(s, Strip::new(key, files.into(), index)) else { return false };
    open(s, v, tx);
    true
}

/// The viewer on the strip's open image, when it's loaded.
fn viewing<S: TimelineHost>(s: &S, strip: Strip) -> Option<Viewing> {
    let f = strip.files.get(strip.at)?;
    let store = s.media_store();
    let fallback = store.image(f).filter(|img| img.error().is_none())?;
    let full = store.image_full(f).unwrap_or_else(|| fallback.clone());
    let natural = store.natural_size(f);
    Some(Viewing { id: strip.id(), name: f.name.clone(), resolution: Resolution::new(full, fallback), natural, dragged: Dragged::File(f.clone()), strip: Some(strip) })
}

/// ← and →: the gallery's previous or next picture, fitted, stopping at either end (Desktop's
/// preview shows one image and has no stepping, `image-preview.tsx`). The viewer takes every
/// bare arrow, so none moves the timeline's focus behind it.
fn arrow<S: TimelineHost>(s: &mut S, ev: &KeyEvent, _: &mut Tx) -> bool {
    let m = ev.mods;
    if m.cmd || m.ctrl || m.alt || m.logo || !matches!(ev.key, Key::Left | Key::Right) {
        return false;
    }
    let Some(strip) = s.media_store().viewing.as_ref().and_then(|v| v.strip.clone()) else { return true };
    // The nearest loaded picture that way: a file that isn't one, or failed, is passed over.
    let at = |j| viewing(s, Strip { at: j, ..strip.clone() });
    let next = if ev.key == Key::Left { (0..strip.at).rev().find_map(at) } else { (strip.at + 1..strip.files.len()).find_map(at) };
    if let Some(v) = next {
        show(s, v);
    }
    true
}

/// Where the viewer shows an image of `size` pixels in a `w`×`h` window at fit: a pixel a point,
/// shrunk to fit inside 56 points top and bottom and 3% of the width (20 to 48 points) at the
/// sides, centred.
pub fn shown(size: Option<[u32; 2]>, w: f32, h: f32) -> [f32; 4] {
    let side = (w * 0.03).clamp(8.0, 36.0) + 12.0;
    let (bw, bh) = ((w - 2.0 * side).max(1.0), (h - 2.0 * 56.0).max(1.0));
    let [iw, ih] = size.map_or([bw, bh], |[a, b]| [a.max(1) as f32, b.max(1) as f32]);
    let k = (bw / iw).min(bh / ih).min(1.0);
    let (sw, sh) = (iw * k, ih * k);
    [(w - sw) / 2.0, (h - sh) / 2.0, sw, sh]
}

/// Rebuilds changed clip controls and steps the open image's zoom glide.
pub fn poll<S: TimelineHost>(s: &mut S, now: f64, tx: &mut Tx) -> bool {
    // Touch the tracked host store only when a clip changes, a full-res lightbox decode lands,
    // or a zoom glide is running.
    let clips = s.media_store().videos.poll(s.active_session_id(), tx);
    if clips { let _ = s.media_store_mut(); }
    let promoted = matches!(s.media_store().viewing.as_ref().map(|v| &v.resolution), Some(Resolution::Decoding { full, .. }) if full.is_ready())
        && s.media_store_mut().viewing.as_mut().is_some_and(|v| v.resolution.poll());
    clips | promoted | (s.media_store().zoom.gliding() && s.media_store_mut().zoom.poll(now, tx))
}

fn close<S: TimelineHost>(s: &mut S, _: &mut Tx) {
    s.media_store_mut().viewing = None;
}

/// Whether the image's place shows whole in the timeline. Then it flies home over the header
/// and composer, as it flew out; a place scrolled under them takes it home inside the
/// timeline's clip, so it slides under the edge instead of landing unseen.
fn home_in_view<S: TimelineHost>(s: &S, cx: &Cx<S>) -> bool {
    let Some(session) = s.active_session_id() else { return false };
    let (Some(home), Some(port)) = (cx.window_layout(HELD), cx.window_layout(&format!("scroll-{session}"))) else { return false };
    home[0] >= port[0] - 0.5 && home[1] >= port[1] - 0.5 && home[0] + home[2] <= port[0] + port[2] + 0.5 && home[1] + home[3] <= port[1] + port[3] + 0.5
}

fn zoom<S: TimelineHost>(s: &mut S) -> &mut ImageZoom {
    &mut s.media_store_mut().zoom
}

/// The open viewer, over the whole window: a scrim, and the preview.
pub fn viewer<S: TimelineHost>(s: &S, p: &Pal, cx: &Cx<S>) -> Vec<El> {
    let Some(v) = &s.media_store().viewing else { return Vec::new() };
    // The preview fills the overlay layer it's drawn in, so it fits the image to its own box. The
    // shell's `window` row is shorter under a plugin's bottom bar, and a story's stage has none
    // (the image then fitted a made-up 1280 × 820 and ran off the stage). Before the preview
    // has a box of its own, the `zoomed` layer it sits in stands in.
    let window = cx.layout_size(rune_kit::image_zoom::KEY).or_else(|| cx.layout_size("zoomed")).or_else(|| cx.layout_size("window")).unwrap_or([1280.0, 820.0]);
    let fade = SpringPair::new(Spring::new(0.15, 0.0), Spring::new(0.12, 0.0));
    let scrim = el().key("image-viewer-scrim").absolute(0.0, 0.0).w_full().h_full().fill(p.scrim).transition(Transition::opacity().springs(fade));
    let shown = v.resolution.shown();
    let natural = match &v.resolution {
        Resolution::Decoding { fallback, .. } => v.natural.or_else(|| fallback.size()),
        Resolution::Ready(img) => img.size().or(v.natural),
    }
    .map_or([1.0, 1.0], |[w, h]| [w as f32, h as f32]);
    // With its place off screen there's nothing to fly from: it fades and grows in instead.
    let arrive = Transition::scale(0.96).springs(fade);
    let image = super::menu::on_image::<S>(el(), v.dragged.clone(), None)
        .key("image-viewer")
        .role("img")
        .label(v.name.clone())
        .matched(v.id.clone())
        .token_motion(flight(s))
        .pixels_fit(shown.pixels(), SurfaceFit::Fill)
        .transition(arrive);
    let store = s.media_store();
    let unclipped = home_in_view(s, cx);
    let preview = Preview::new(zoom::<S>, natural, image)
        .icons(&rune_theme_opencode::icons::MINUS, &rune_theme_opencode::icons::PLUS)
        .labels(
            opencode_i18n::t!("ui.imagePreview.zoomOut"),
            opencode_i18n::t!("ui.imagePreview.zoomToFit"),
            opencode_i18n::t!("ui.imagePreview.zoomIn"),
            opencode_i18n::t!("ui.common.close"),
        )
        .format_level(|n| opencode_i18n::percent(f64::from(n) / 100.0))
        .controls_transition(Transition::opacity().springs(fade))
        // A zoomed image keeps the arrow: official's grab and grabbing there
        // (`packages/ui/src/components/image-preview.css:134,139`) are reset to the default
        // cursor (`packages/desktop/src/renderer/styles.css:7-8`). Dragging still pans.
        .pan_cursors(false)
        .on_close(move |s: &mut S, tx: &mut Tx| {
            if unclipped {
                tx.unclipped_flights();
            }
            close(s, tx)
        })
        // Arrows step the gallery; every other key goes on to the preview's own zoom keys.
        .on_key(arrow::<S>)
        .build(&store.zoom, crate::palette::kit(p), window, cx.is_reduced_motion())
        .role("dialog")
        .label(v.name.clone());
    vec![scrim, preview]
}

