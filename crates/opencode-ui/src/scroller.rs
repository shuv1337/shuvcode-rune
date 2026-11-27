//! A vertical scroll container with the bar `@opencode/ui` gives its Desktop counterpart. Desktop
//! styles every plain `overflow: auto` box with `scrollbar.css` (`packages/ui/src/styles/scrollbar.css:25-63`),
//! so [`scroller`] gives that bar unless the box names another: [`Bar::ScrollView`] or
//! [`Bar::MenuScroll`] where Desktop wraps the box in that component, or [`Bar::Hidden`] where it
//! hides the bar (`no-scrollbar`, `[scrollbar-width:none]`) or the box has no counterpart that
//! scrolls. Build a scroll container here, not with `El::scroll`, so a new one gets Desktop's bar
//! by default and one without it says why.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use rune::{Color, El, ScrollSpec, Tx, col};
use rune_kit::{ScrollBar, ScrollBarStyle};

use crate::palette::{Pal, kit};

/// The bar a scroll container has, as its Desktop counterpart does.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Bar {
    /// `scrollbar.css`, every plain `overflow: auto` box's: while the content overflows the bar
    /// takes an 8 point gutter from the box's width ([`GUTTER`]), and a 4 point `border-muted` pill
    /// shows in it only while the pointer is over the box, `border-strong` under the pointer or
    /// while dragged.
    #[default]
    Native,
    /// `ScrollView`'s overlay thumb (`scroll-view.css`): over the content, shown while the pointer
    /// is over the box or it scrolls, fading.
    ScrollView,
    /// `MenuScroll`'s thumb (`menu-scroll.css`), a menu's or dialog list's: shown only while the
    /// list scrolls or the thumb is under the pointer.
    MenuScroll,
    /// A `ScrollView` thumb in a track the page places (`thumbContainer`, `thumbHoverTarget`):
    /// [`ScrollBarStyle::scroll_view`] with its [`ScrollBarStyle::track_offset`] and
    /// [`ScrollBarStyle::hover_target`].
    Styled(ScrollBarStyle),
    /// `@pierre/trees`' own bar (its `style.js`: a file tree scrolls itself in its shadow root,
    /// where `scrollbar.css` doesn't reach): a [`TREE_GUTTER`] kept whether or not the rows
    /// overflow (`scrollbar-gutter: stable`), a 4 point pill inside a 1 point clear border with no
    /// track margin, 17 long at least, in `color-mix(in lab, fg 25%, bg)` (`--trees-scrollbar-thumb`
    /// from the tree's `--trees-fg` and `--trees-bg`), shown only while the pointer is over the tree.
    Tree { fg: Color, bg: Color },
    /// No bar, as Desktop hides it.
    Hidden,
}

/// The width `scrollbar.css`'s bar takes from layout while the content overflows (`--scrollbar-size`).
pub const GUTTER: f32 = 8.0;

/// The width [`Bar::Tree`]'s bar keeps at the tree's end (`--trees-scrollbar-gutter`).
pub const TREE_GUTTER: f32 = 6.0;

thread_local! {
    /// Each [`Bar::Native`] container's last measured viewport and content heights, by key.
    static FITS: RefCell<HashMap<Rc<str>, [f32; 2]>> = RefCell::default();
    /// The view's device pixels to a point, as of [`set_device_scale`].
    static SCALE: Cell<f32> = const { Cell::new(0.0) };
}

/// The device pixels to a point of the view being built (its `Env::scale`), set once per view:
/// [`native_bar`]'s thumb lands on whole ones, as Chromium places a native thumb.
/// Last scale written by `set_device_scale` on any UI thread, so background page workers
/// (`prepare_images`) decode thumbnails at the real display scale instead of falling back to 1.0.
static LAST_SCALE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn set_device_scale(scale: f32) {
    SCALE.set(scale);
    if scale.is_finite() && scale > 0.0 {
        LAST_SCALE.store(scale.to_bits(), std::sync::atomic::Ordering::Relaxed);
    }
}

/// The device pixels to a point of the view being built, 1 before [`set_device_scale`].
pub fn device_scale() -> f32 {
    let s = SCALE.get();
    if s > 0.0 {
        return s;
    }
    let last = f32::from_bits(LAST_SCALE.load(std::sync::atomic::Ordering::Relaxed));
    if last > 0.0 { last } else { 1.0 }
}

/// The points a `w` pt CSS border takes at the view's scale: Chromium lays it out on whole
/// device pixels, one at least, as `El::border_layout` does.
pub fn border_width(w: f32) -> f32 {
    let s = SCALE.get();
    if s <= 0.0 {
        return w;
    }
    (w * s + 1.0 / 64.0).floor().max(1.0) / s
}

/// `scrollbar.css`'s bar, in the room its box keeps for it: its thumb's length rounded and its
/// place truncated to the device pixels, as Chromium's (`ScrollbarTheme::ThumbPosition`).
pub fn native_bar(p: &Pal) -> ScrollBarStyle {
    ScrollBarStyle::native(p.border_muted, p.q.border_strong).inside().device_pixels(SCALE.get())
}

/// The gutter [`Bar::Native`] container `key` takes from its content's width now ([`GUTTER`] while it
/// overflows, as of its last layout), for content laid out at a set width.
pub fn gutter(key: &str) -> f32 {
    if FITS.with_borrow(|f| f.get(key).is_some_and(|fit| overflowing(*fit))) { GUTTER } else { 0.0 }
}

/// The [`Bar::Native`] containers whose content overflows now, by key, sorted: what a launch
/// hands to [`expect_overflow`] next time.
pub fn overflowing_keys() -> Vec<String> {
    let mut keys: Vec<String> = FITS.with_borrow(|f| f.iter().filter(|(_, fit)| overflowing(**fit)).map(|(key, _)| key.to_string()).collect());
    keys.sort();
    keys
}

/// Before a view's first frame: the [`Bar::Native`] containers `keys` overflowed when the app
/// last ran, so each starts with its gutter. A container learns whether it overflows from its
/// first layout and takes the gutter the frame after: every row of a long list narrowed by
/// [`GUTTER`] in the launch's second frame. Its first layout still decides: one that fits now
/// gives the gutter back.
pub fn expect_overflow(keys: impl IntoIterator<Item = String>) {
    FITS.with_borrow_mut(|f| {
        for key in keys {
            // A viewport of any height and content past it: each height, as it's measured,
            // replaces its half, and neither alone says it fits.
            f.entry(Rc::from(key)).or_insert([1.0, f32::INFINITY]);
        }
    });
}

/// A scroll container keyed `key`, built by [`Scroller::build`].
pub struct Scroller {
    key: Rc<str>,
    spec: ScrollSpec,
    bar: Bar,
    stable: bool,
}

/// A vertical scroll container keyed `key`, with `scrollbar.css`'s bar unless [`Scroller::bar`]
/// says otherwise.
pub fn scroller(key: impl Into<String>) -> Scroller {
    Scroller { key: Rc::from(key.into()), spec: ScrollSpec { stick_bottom: false }, bar: Bar::Native, stable: false }
}

impl Scroller {
    /// Keeps to its bottom while it's there, as a log does.
    pub fn stick_bottom(mut self) -> Self {
        self.spec.stick_bottom = true;
        self
    }

    /// [`Bar::Native`]'s gutter is there whether or not the content overflows
    /// (`scrollbar-gutter: stable`), so text doesn't rewrap when it starts to scroll.
    pub fn stable_gutter(mut self) -> Self {
        self.stable = true;
        self
    }

    /// Desktop's counterpart has `bar`, not `scrollbar.css`'s.
    pub fn bar(mut self, bar: Bar) -> Self {
        self.bar = bar;
        self
    }

    /// No bar, as Desktop hides it ([`Bar::Hidden`]), for a view without a palette.
    pub fn hidden(self, viewport: El, content: El) -> El {
        viewport.key(self.key.to_string()).scroll(self.spec).child(content)
    }

    /// `viewport` (sized and styled, its fades and handlers set; the key and the scroll are set
    /// here) scrolling `content`. A bar wraps the viewport in an area that takes its size and
    /// place, so set everything on `viewport` first.
    pub fn build<S: 'static>(self, p: &Pal, viewport: El, content: El) -> El {
        let viewport = viewport.key(self.key.to_string()).scroll(self.spec);
        match self.bar {
            Bar::Native => native::<S>(p, self.key, self.stable, viewport, content),
            Bar::ScrollView => viewport.child(content).scroll_bar_styled(ScrollBarStyle::scroll_view(kit(p))),
            Bar::MenuScroll => viewport.child(content).scroll_bar_styled(ScrollBarStyle::menu_scroll(p.border_muted, p.q.border_strong)),
            Bar::Styled(style) => viewport.child(content).scroll_bar_styled(style),
            Bar::Tree { fg, bg } => viewport.child(col().w_full().pr(TREE_GUTTER).child(content)).scroll_bar_styled(tree_bar(fg, bg)),
            Bar::Hidden => viewport.child(content),
        }
    }
}

/// [`Bar::Tree`]'s look. Its least thumb is Chromium's for that bar, as measured in Desktop.
fn tree_bar(fg: Color, bg: Color) -> ScrollBarStyle {
    let thumb = crate::palette::mix_lab(bg, fg, 0.25);
    ScrollBarStyle::native(thumb, thumb).inside().webkit_parts(TREE_GUTTER, 1.0, 0.0).min_thumb(17.0).device_pixels(SCALE.get())
}

/// `scrollbar.css`'s bar: the content's 8 point gutter while it overflows, measured from both
/// heights, and the bar in it. Only a change in whether it overflows runs the view again.
fn native<S: 'static>(p: &Pal, key: Rc<str>, stable: bool, viewport: El, content: El) -> El {
    let bar = native_bar(p);
    if stable {
        return viewport.child(col().w_full().pr(GUTTER).child(content)).scroll_bar_styled(bar);
    }
    let overflows = FITS.with_borrow(|f| f.get(&key).is_some_and(|fit| overflowing(*fit)));
    viewport
        .on_layout_tx(measure::<S>(key.clone(), 0))
        .child(col().w_full().pr(if overflows { GUTTER } else { 0.0 }).on_layout_tx(measure::<S>(key, 1)).child(content))
        .scroll_bar_styled(bar)
}

/// Notes height `slot` (0 the viewport's, 1 the content's) of container `key`; a layout that
/// doesn't change whether it overflows is live only (inside a viewport, that's every scroll frame).
fn measure<S: 'static>(key: Rc<str>, slot: usize) -> impl Fn(&mut S, [f32; 4], &mut Tx) + 'static {
    move |_, r, tx| {
        let flipped = FITS.with_borrow_mut(|f| {
            let fit = f.entry(key.clone()).or_insert([0.0; 2]);
            let was = overflowing(*fit);
            fit[slot] = r[3];
            was != overflowing(*fit)
        });
        if !flipped {
            tx.live_only();
        }
    }
}

fn overflowing([view, content]: [f32; 2]) -> bool {
    view > 0.0 && content - view > 0.5
}

/// Where a timeline stood, to keep across a restart: its session, the first row showing and how
/// far below the viewport's top that row's top stood, and whether that was more than a screen up.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub session: String,
    pub row: String,
    pub at: f32,
    pub far: bool,
}
