//! A diagram in a reply opens full size: the expand button on its card (shown on hover), a
//! double-click on it, or Space while the pointer is over it grows the card out of its place in
//! the reply to fill the window over a scrim. There a pinch, ⌘+ and ⌘− zoom it, two fingers or
//! a drag pan it, ⌘0 fits it again, and Esc, Space or a click outside shrinks it back into its
//! place. The column draws a wide diagram whole (`markdown_style`'s `diagram_min_scale(0.0)`),
//! however small, so this is where it's read.

pub mod lightbox;

use std::cell::Cell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use rune::{El, Icon, Key, KeyEvent, Tx, el, icon};
use rune_kit::{DiagramFrame, MarkdownStyle};

use crate::TimelineHost;
use crate::focus::FocusExt;
use crate::keymap::Bind;
use crate::palette::Pal;
use crate::style::INSTANT;

pub use lightbox::view;

static EXPAND_ICON: Icon = Icon {
    name: "diagram-expand",
    view: 16.0,
    body: r#"<path d="M9.5 2.5H13.5V6.5M13.5 2.5L9 7M6.5 13.5H2.5V9.5M2.5 13.5L7 9" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#,
};

/// A diagram as the reply last laid it out: where its card is (window points) and what draws
/// it again in the same shape.
#[derive(Clone)]
struct Seen {
    rect: [f32; 4],
    source: Rc<str>,
    style: Rc<MarkdownStyle>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    /// Mounted over its card, the frame before it grows.
    Opening,
    Open,
    /// Flying back into its card; gone at `until` on the app clock.
    Closing { until: f64 },
}

struct Open {
    id: u64,
    seen: Seen,
    phase: Phase,
    /// Times the fitted size.
    zoom: f32,
    /// Points from the centred place.
    pan: [f32; 2],
    gesture: Gesture,
}

/// The press under way on the open diagram, as Desktop's preview zoom has it
/// (`ui/src/components/image-zoom-machine.ts:10-19`, `PreviewZoomGesture`). Rune's drag reports
/// where its press began and whether it moved, so only the pan keeps state here.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Gesture {
    #[default]
    Idle,
    /// A drag pans it; `last` is where the pointer was at its last event.
    Panning { last: [f32; 2] },
}

/// The diagrams the timeline shows and the one open full size.
#[derive(Default)]
pub struct Diagrams {
    seen: HashMap<u64, Seen>,
    hovered: Option<u64>,
    open: Option<Open>,
    /// What the last view fitted the open diagram to: its scale at zoom 1, the window and the
    /// card's size, in the zoomed shell's points.
    pub fit: Cell<lightbox::Fit>,
}

thread_local! {
    /// The diagram open full size, whose card in the reply hides while it is.
    static SHOWN: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Called at the top of each view, before the timeline builds.
pub fn prepare<S: TimelineHost>(s: &S) {
    SHOWN.set(s.diagrams().open.as_ref().map(|o| o.id));
    s.media_store().begin_frame();
}

fn id_of(source: &str) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    source.hash(&mut h);
    h.finish()
}

impl Diagrams {
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The open diagram's id, when reply `body` has it: a row memo's version, so its card
    /// hides and comes back.
    pub fn hidden_in(&self, body: &str) -> Option<u64> {
        self.open.as_ref().filter(|o| body.contains(&*o.seen.source)).map(|o| o.id)
    }

    /// Releases cached diagram snapshots; visible cards re-register on the next view pass.
    pub fn rest(&mut self) {
        let open_id = self.open.as_ref().map(|o| o.id);
        self.seen.retain(|id, _| open_id == Some(*id));
    }
}

/// Opens diagram `id` full size, from where the reply last laid it out. False when no card of
/// it has been laid out.
pub fn open<S: TimelineHost>(s: &mut S, id: u64, tx: &mut Tx) -> bool {
    let Some(seen) = s.diagrams().seen.get(&id).cloned() else { return false };
    tx.instant();
    s.diagrams_mut().open = Some(Open { id, seen, phase: Phase::Opening, zoom: 1.0, pan: [0.0, 0.0], gesture: Gesture::Idle });
    true
}

/// Opens the diagram whose source is `source` (the storybook's and motion gate's way in).
pub fn open_source<S: TimelineHost>(s: &mut S, source: &str, tx: &mut Tx) -> bool {
    let id = s.diagrams().seen.iter().find(|(_, v)| v.source.trim() == source.trim()).map(|(id, _)| *id);
    id.is_some_and(|id| open(s, id, tx))
}

/// Shrinks the open diagram back into its card.
pub fn close<S: TimelineHost>(s: &mut S, tx: &mut Tx) {
    if let Some(left) = close_later(s) {
        tx.animated();
        tx.wake_after(std::time::Duration::from_secs_f64(left));
    }
}

/// [`close`] without a transaction (the modal's dismiss): the seconds until it lands, or
/// `None` when it was already on its way.
fn close_later<S: TimelineHost>(s: &mut S) -> Option<f64> {
    let left = f64::from(lightbox::flight(s).duration) * 1.6;
    let until = s.clock() + left;
    let seen_rect = s.diagrams().open.as_ref().and_then(|o| s.diagrams().seen.get(&o.id).map(|v| v.rect));
    let o = s.diagrams_mut().open.as_mut().filter(|o| !matches!(o.phase, Phase::Closing { .. }))?;
    if let Some(rect) = seen_rect {
        o.seen.rect = rect;
    }
    o.phase = Phase::Closing { until };
    o.gesture = Gesture::Idle;
    Some(left)
}

/// Drops the diagram that has flown back, once it has landed.
pub fn poll<S: TimelineHost>(s: &mut S, now: f64, tx: &mut Tx) -> bool {
    let Some(Open { phase: Phase::Closing { until }, .. }) = &s.diagrams().open else { return false };
    if now < *until {
        tx.wake_after(std::time::Duration::from_secs_f64(until - now));
        return false;
    }
    s.diagrams_mut().open = None;
    tx.instant();
    true
}

/// Space opens the diagram under the pointer; while one is open, Esc and Space close it and
/// ⌘+, ⌘− and ⌘0 zoom it rather than the app.
pub fn key<S: TimelineHost>(s: &mut S, ev: &KeyEvent, tx: &mut Tx) -> bool {
    let space = matches!(&ev.key, Key::Text(t) if t == " ") && !(ev.mods.cmd || ev.mods.alt || ev.mods.ctrl);
    if s.diagrams().open.is_none() {
        return space && tx.focused().is_none() && s.diagrams().hovered.is_some_and(|id| open(s, id, tx));
    }
    match &ev.key {
        Key::Escape => close(s, tx),
        _ if space => close(s, tx),
        _ if Bind::ZoomIn.matches_tx(ev, tx) => lightbox::zoom_step(s, 1.25, tx),
        _ if Bind::ZoomOut.matches_tx(ev, tx) => lightbox::zoom_step(s, 0.8, tx),
        _ if Bind::ZoomReset.matches_tx(ev, tx) => lightbox::fit(s, tx),
        _ => return false,
    }
    true
}

/// The reply's diagram card `card`, with its expand button; `style` draws it again full size.
pub fn frame<S: TimelineHost>(p: &Pal, card: El, d: &DiagramFrame<'_>, style: &Rc<MarkdownStyle>) -> El {
    let id = id_of(d.source);
    let hidden = SHOWN.get() == Some(id);
    let wrap = el().w_full().opacity(if hidden { 0.0 } else { 1.0 }).opacity_motion(INSTANT).child(card);
    if d.open {
        return wrap;
    }
    let (source, style) = (Rc::<str>::from(d.source), style.clone());
    wrap.hover_group_focus()
        .on_layout_tx(move |s: &mut S, rect, tx| {
            s.diagrams_mut().seen.insert(id, Seen { rect, source: source.clone(), style: style.clone() });
            tx.live_only();
        })
        .on_hover_change(move |s: &mut S, on, tx| {
            if on {
                s.diagrams_mut().hovered = Some(id);
            } else if s.diagrams().hovered == Some(id) {
                s.diagrams_mut().hovered = None;
            }
            tx.live_only();
        })
        .on_double_press_tx(move |s: &mut S, tx| {
            open(s, id, tx);
        })
        .child(button::<S>(p, id).pin(None, Some(8.0), Some(8.0), None))
}

fn button<S: TimelineHost>(p: &Pal, id: u64) -> El {
    crate::style::hover_reveal(
        el().key("diagram-open")
            .size(25.0, 25.0)
            .radius(5.0)
            .center()
            .focus_default(p)
            .fill(p.bg_base)
            .border(0.5, p.border_muted)
            .hover(p.hover)
            .press(p.pressed)
            .hover_motion(INSTANT, INSTANT)
            .role("button")
            .label_static(opencode_i18n::t!("rune.diagram.openFullSize"))
            .help(opencode_i18n::t!("rune.diagram.openFullSize"))
            .on_press_tx(move |s: &mut S, tx| {
                open(s, id, tx);
            })
            .child(icon(&EXPAND_ICON, 13.0).color(p.icon_muted)),
    )
}
