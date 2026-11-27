//! "Jump to latest": the arrow over the timeline's bottom edge while you read more than a screen
//! up (`session/timeline/virtualizer.tsx`, `interaction.ts`). Pressing it glides to the end, and
//! the timeline follows new content again once it lands.
use std::collections::HashMap;

use rune::{Anim, El, Icon, Tx, col, el, icon, row, spacer};
use rune_kit::ScrollBar;

use crate::TimelineHost;
use crate::focus::FocusExt;
use crate::follow::{self, Effect, Event, Follow};
use crate::returning::{self, Returning};
use crate::palette::Pal;
use crate::style::{DOCK_SIDE, activity_ring};

/// Where the pills over the composer sit (Settings › Preferences › Background activity,
/// `useComposerLaneAlignment`): at the lane's start, centred, or at its end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

/// Desktop's `jumpThreshold` (`interaction.ts:125`): more than the viewport's height, and at
/// least 400 above the end. Desktop's viewport runs under the header and the composer, so its
/// height is the pane's (`TimelineHost::timeline_frame`), not the rows' window above the composer.
const THRESHOLD: f32 = 400.0;
/// Centred, its bottom edge sits this far above the timeline's (`bottom: inset + 32px`).
const LIFT: f32 = 32.0;
/// At the right, the gap the jump button and the subagent capsule leave above the composer
/// (the pill lane's `pb-2.5`, `composer-lane.tsx:36`).
pub const GAP: f32 = 10.0;
/// A composer pill's height (`[data-composer-pill]`, `composer-lane.css:7`).
pub const CHIP_H: f32 = 32.0;
/// The capsule's lane: its chip and that gap, kept clear under the timeline's last row.
pub fn chip_height() -> f32 { if crate::style::hover_none() { 44.0 } else { CHIP_H } }

/// The room the pills take over the composer: their lane while any shows.
pub fn lane(pills: bool) -> f32 {
    if pills { chip_height() + GAP } else { 0.0 }
}

/// The timeline's fade over the composer (p248): transparent at the composer's top edge, under
/// the pills' `lane`, and whole the "Composer fade" knob above them, linear in between.
pub fn fade<S: TimelineHost>(s: &S, lane: f32) -> f32 {
    lane + s.knobs().get(&crate::knobs::COMPOSER_FADE)
}

static ARROW_DOWN: Icon = Icon {
    name: "arrow-down",
    view: 16.0,
    body: r#"<path d="M12.3333 8.66665L8 13L3.66667 8.66665M8 12.6667V2.83332" stroke="currentColor" stroke-linecap="square"/>"#,
};

/// Where each session's timeline stands against its end.
#[derive(Default)]
pub struct Ends {
    /// Each session's follow state; one not listed follows its end ([`Follow::Following`]).
    follow: HashMap<String, Follow>,
    /// Where each timeline was last drawn, for its place when it shows again.
    pub recall: crate::recall::Recall,
    /// The place coming back as its session opens again (`returning`).
    returning: Returning,
    /// Places read in an earlier run, for timelines this run hasn't drawn yet.
    saved: HashMap<String, (String, f32)>,
    /// The place each timeline was last held at through a settle ([`held`]).
    held: std::cell::RefCell<HashMap<String, (String, f32)>>,
}

pub use opencode_ui::scroller::Place;

impl Ends {
    /// Whether session `id`'s timeline stands more than a screen up, where Jump to latest shows.
    pub fn far(&self, id: &str) -> bool {
        self.follow.get(id).is_some_and(Follow::far)
    }

    /// Whether session `id`'s timeline follows its end, where what lands shows.
    pub fn at_end(&self, id: &str) -> bool {
        self.follow.get(id).is_none_or(Follow::following)
    }

    /// A timeline that mounts afresh starts on its end.
    pub fn forget(&mut self, id: &str) {
        self.follow.remove(id);
        self.saved.remove(id);
        self.held.borrow_mut().remove(id);
        self.recall.forget(id);
    }

    /// Bounds scroll/recall state to active sessions plus up to `max_closed` closed sessions so
    /// Undo Close Tab (⇧⌘T) preserves scroll position without unbounded growth.
    pub fn retain_bounded(&mut self, keep: &dyn Fn(&str) -> bool, max_closed: usize) {
        let mut closed: Vec<String> = self
            .follow
            .keys()
            .chain(self.saved.keys())
            .filter(|id| !keep(id))
            .cloned()
            .collect();
        closed.sort();
        closed.dedup();
        if closed.len() <= max_closed {
            return;
        }
        let keep_closed: std::collections::HashSet<&str> = closed[closed.len() - max_closed..].iter().map(String::as_str).collect();
        let kept = |id: &str| keep(id) || keep_closed.contains(id);
        self.follow.retain(|id, _| kept(id));
        self.saved.retain(|id, _| kept(id));
        self.held.borrow_mut().retain(|id, _| kept(id));
        self.recall.retain(&kept);
    }

    /// Where session `id`'s timeline was read when it last showed, scrolled up: its first row
    /// showing and how far below the top that row's top stood. `None` while it follows its end.
    pub fn place(&self, id: &str) -> Option<(String, f32)> {
        if self.at_end(id) { return None; }
        self.recall.place(id).or_else(|| self.saved.get(id).cloned())
    }

    /// Places from an earlier run (`places`): each timeline stands there, not on its end, until
    /// it's drawn and read again.
    pub fn restore(&mut self, places: Vec<Place>) {
        for p in places {
            self.follow.insert(p.session.clone(), Follow::Detached { far: p.far });
            self.saved.insert(p.session, (p.row, p.at));
        }
    }

    /// Where each of `sessions` stands, for those not on their end.
    pub fn places<'a>(&self, sessions: impl Iterator<Item = &'a str>) -> Vec<Place> {
        sessions.filter_map(|id| self.place(id).map(|(row, at)| Place { session: id.to_owned(), row, at, far: self.far(id) })).collect()
    }

    /// Whether session `id`'s place is coming back, waiting for its row (`returning`).
    pub fn due(&self, id: &str) -> bool {
        self.due_row(id).is_some()
    }

    /// The row session `id`'s coming place waits for.
    pub fn due_row(&self, id: &str) -> Option<&str> {
        self.returning.due().filter(|(due, _)| *due == id).map(|(_, row)| row)
    }

    /// A poll at `now` found session `id`'s row missing: true while its place waits on (read the
    /// page above, and poll again soon), false once it's given up and the timeline follows its end.
    pub fn wait(&mut self, id: &str, now: f64) -> bool {
        match returning::step(&mut self.returning, returning::Event::Missing { session: id, now }) {
            Some(returning::Effect::ReadOn) => true,
            Some(returning::Effect::GiveUp) => {
                self.forget(id);
                false
            }
            _ => false,
        }
    }

    /// A page came for session `id` without its place's row: the place waits again from the next
    /// poll, as pages read on up to it.
    pub fn progress(&mut self, id: &str) {
        returning::step(&mut self.returning, returning::Event::Page { session: id });
    }

    /// Session `id` is opened again. Read scrolled up, it comes back to its place once its
    /// transcript has the place's row ([`Ends::land`]), with Jump to latest as it was; otherwise
    /// its timeline mounts afresh on its end and follows it.
    pub fn reopen(&mut self, id: &str) {
        let place = self.place(id);
        returning::step(&mut self.returning, returning::Event::Reopen { session: id, place: place.as_ref().map(|(row, at)| (row.as_str(), *at)) });
        if place.is_none() {
            self.forget(id);
        }
    }

    /// Session `id`'s transcript has its coming place's row: put the place back in this commit,
    /// so the frame that first draws its rows draws them there, a virtual row out of the mounted
    /// window included (`Tx::scroll_key_to`).
    pub fn land(&mut self, id: &str, tx: &mut Tx) {
        let Some(returning::Effect::Land { row, at }) = returning::step(&mut self.returning, returning::Event::Here { session: id }) else { return };
        self.held.borrow_mut().insert(id.to_owned(), (row.clone(), at));
        tx.scroll_key_to(row, at);
    }

    /// Steps session `id`'s follow state (`follow::step`); `None` steps a timeline that belongs
    /// to no session as a detached one. Returns the hold to make.
    pub fn step(&mut self, id: Option<&str>, event: Event) -> Option<Effect> {
        let Some(id) = id else { return follow::step(Follow::Detached { far: false }, event).1 };
        let (next, effect) = follow::step(self.follow.remove(id).unwrap_or_default(), event);
        if next != Follow::Following {
            self.follow.insert(id.to_owned(), next);
        }
        effect
    }

    /// Whether `event` would change session `id`'s follow state. A viewport reports where it
    /// stands after every scroll; a report that changes nothing mustn't write the state the
    /// timeline's region reads, which would build every row again.
    /// A place coming back (`returning`) changes nothing: while it waits, the viewport stands on
    /// its loading look's end, which says nothing about where its reader is.
    pub fn moves(&self, id: &str, event: Event) -> bool {
        if self.due(id) {
            return false;
        }
        let now = self.follow.get(id).cloned().unwrap_or_default();
        follow::step(now.clone(), event).0 != now
    }

    pub fn set_far(&mut self, id: &str, far: bool) {
        self.step(Some(id), Event::Far(far));
    }

    pub fn set_at_end(&mut self, id: &str, at: bool) {
        self.step(Some(id), Event::AtEnd(at));
    }
}

/// `Tx::settle` for a change that lands what's moving, which also carries every stick-to-bottom
/// viewport to its end. The open timeline read up from its end (its reader's place, or one kept
/// from the last run) isn't settled: the change applies at once (`Tx::instant`) and the place is
/// put back in the same commit, so neither the carry nor a row the commit hasn't laid out yet can
/// take it to its end. Motion has a cause: the reader, never a change landing.
pub fn settle<S: TimelineHost>(s: &S, tx: &mut Tx) {
    settle_held(tx, held(s).as_ref());
}

/// The open timeline's place a settle must keep: its session, its first row showing and where.
/// A place read again within a point of the one kept last is that one, so holding it twice puts
/// it on the same pixel (the drawn rect it's read from is snapped to pixels; the offset isn't).
pub fn held<S: TimelineHost>(s: &S) -> Option<(String, String, f32)> {
    let id = s.active_session_id()?;
    let (row, at) = s.ends().place(id)?;
    let mut last = s.ends().held.borrow_mut();
    let at = match last.get(id) {
        Some((r, a)) if *r == row && (a - at).abs() < 1.0 => *a,
        _ => at,
    };
    last.insert(id.to_owned(), (row.clone(), at));
    Some((id.to_owned(), row, at))
}

/// [`settle`] with the place already read ([`held`]).
pub fn settle_held(tx: &mut Tx, held: Option<&(String, String, f32)>) {
    let Some((_, row, at)) = held else {
        tx.settle();
        return;
    };
    tx.instant();
    tx.scroll_key_to(row.clone(), *at);
}

/// Records whether session `id`'s timeline stands far enough up to show the button.
fn far<S: TimelineHost>(id: &str) -> impl Fn(&mut S, bool) -> bool + 'static {
    let id = id.to_owned();
    move |s: &mut S, far| {
        let moved = s.ends().moves(&id, Event::Far(far));
        if moved { s.ends_mut().set_far(&id, far); }
        moved
    }
}

/// Records whether session `id`'s timeline shows its end.
fn at_end<S: TimelineHost>(id: &str) -> impl Fn(&mut S, bool) -> bool + 'static {
    let id = id.to_owned();
    move |s: &mut S, at| {
        let moved = s.ends().moves(&id, Event::AtEnd(at));
        if moved { s.ends_mut().set_at_end(&id, at); }
        moved
    }
}

/// The button as it stands: frosted glass, the timeline blurred at 2 pt under bg-base at 92%
/// (`backdrop-blur-[2px]`), raised, with a soft bg-base halo under it (`0 2px 8px`).
pub fn face(p: &Pal, radius: f32) -> El {
    let [far, near, ring, edge] = p.raised;
    let mut button = el()
        .key("jump-latest")
        .role("button")
        .label_static(opencode_i18n::t!("session.messages.jumpToLatest"))
        // `w-8 h-7 rounded-lg` (`virtualizer.tsx:1141`).
        .size(32.0, 28.0)
        .radius(radius)
        .center();
    if !crate::style::permanent_cpu_canvas() {
        button = button.backdrop_blur(2.0);
    }
    button
        .fill(p.frost)
        // Bottom first: CSS paints its first shadow on top.
        .shadow(2.0, 8.0, p.bg_base)
        .box_shadow(edge)
        .box_shadow(ring)
        .box_shadow(near)
        .box_shadow(far)
        .focus_default(p)
        .hover(p.hover)
        .press(p.pressed)
        .hover_motion(Anim::INSTANT, Anim::INSTANT)
        .child(icon(&ARROW_DOWN, 16.0).color(p.text))
}

/// Wraps the timeline's `scroll` with its bottom fade, scroll-edge callbacks, and styled
/// scrollbar (`timeline-fade`). Callers that keep `pills` in a sibling region can memoize this
/// subtree independently of pill and job updates (`ours.md` §3.2) and pass it to [`over_scroll`].
pub fn timeline_scroll<S: TimelineHost>(s: &S, p: &Pal, id: &str, scroll: El, has_pills: bool) -> El {
    let edge = fade(s, lane(has_pills));
    col()
        .key("timeline-fade")
        .grow(1.0)
        .shrink()
        .w_full()
        .clip_fade_linear(0.0, 0.0, 0.0, edge)
        .child(
            scroll
                .on_far_from_bottom_if(THRESHOLD, s.timeline_frame(), far::<S>(id))
                .on_at_bottom_if(at_end::<S>(id))
                .scroll_bar_styled(crate::palette::scroll_bar(p).hover_target()),
        )
}

/// The timeline `scroll` with the button and the `pills` (the subagent capsule, the todo pill)
/// floating over its bottom edge. The timeline runs down to the composer and fades out into its
/// top edge, under the pills (p248); at its end its last row stands clear over them (t1129), and
/// they ride up as the docks under it grow. At the right (the ⌘⇧K Composer default) the button
/// ends where the composer ends, on the capsule's row; centred, it's Desktop's.
///
/// `top`: the height of the session header stuck over the timeline's top, which the
/// older-history ring sits under.
pub fn over<S: TimelineHost>(s: &S, p: &Pal, id: &str, scroll: El, pills: Vec<El>, top: f32) -> El {
    let scroll_fade = timeline_scroll(s, p, id, scroll, !pills.is_empty());
    over_scroll(s, p, id, scroll_fade, pills, top).scroll_bar_hover(&format!("scroll-{id}"))
}

/// [`over`] taking an already-built `timeline-fade` subtree from [`timeline_scroll`].
pub fn over_scroll<S: TimelineHost>(s: &S, p: &Pal, id: &str, scroll_fade: El, pills: Vec<El>, top: f32) -> El {
    let shown = s.ends().far(id);
    // Timeline knobs: 0.15 s in, 0.11 s out, rising 4 pt as it shows (`translate-y-1`).
    let curve = crate::knobs::jump(s.knobs(), shown);
    let rise = if shown || s.reduced_motion() { 0.0 } else { s.knobs().get(&crate::knobs::JUMP_RISE) };
    let target = format!("scroll-{id}");
    let session = id.to_owned();
    let mut button = face(p, s.knobs().get(&crate::knobs::CAPSULE_RADIUS))
        .opacity(if shown { 1.0 } else { 0.0 })
        .opacity_motion(curve)
        .offset(0.0, rise)
        .offset_motion(curve)
        .on_press_tx(move |s: &mut S, tx: &mut Tx| {
            // Instant snap under reduced motion (`virtualizer.tsx:1168`); keyboard activation
            // moves focus to the composer before this button becomes inert (`virtualizer.tsx:1171`).
            if s.reduced_motion() {
                s.ends_mut().set_at_end(&session, true);
                tx.scroll_to_bottom_instant(target.clone());
            } else {
                tx.scroll_to_bottom(target.clone());
            }
            if tx.by_keys() {
                s.focus_composer(tx);
            }
        });
    if !shown {
        button = button.inert();
    }
    // A pill's height whatever shows, so the 28 pt button sits where it does beside the 32 pt
    // capsule and doesn't move 2 pt when one comes or goes.
    let lane = row().key("activity-lane").w_full().max_w(crate::style::dock_w()).min_h(chip_height()).center_y();
    let lane = match s.lane_align() {
        Align::Start => lane.children(pills).child(spacer()),
        Align::End => lane.child(spacer()).children(pills),
        // Centred on the lane whatever sits at its end: over it, out of its flow.
        Align::Center => lane.child(spacer()).children((!pills.is_empty()).then(|| row().key("activity-centre").pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0)).center().children(pills))),
    };
    // Pills at the right end the lane where Desktop's do, at the composer's end, so the button
    // stands centred above the composer there, as Desktop's always does.
    let (lane, centred) = if s.knobs().get(&crate::knobs::JUMP_PLACE) as usize == 1 || s.lane_align() == Align::End {
        (lane, Some(row().key("jump-centre").pin(Some(0.0), None, Some(0.0), Some(LIFT)).center().child(button)))
    } else {
        (lane.child(button), None)
    };
    col()
        .key("timeline-area")
        .grow(1.0)
        .shrink()
        .w_full()
        // Its own bottom fade, a fixed one around the scroll's (which a wider one outranks):
        // under the timeline's last row at its end there's only padding.
        // Rune's scroll bar, under the jump button and pills, which keep their presses where
        // they overlap it.
        .child(scroll_fade)
        .child(row().key("jump-lane").pin(Some(0.0), None, Some(0.0), Some(GAP)).px(DOCK_SIDE).center().child(lane))
        .children(centred)
        // Over the timeline's top edge, not in it: a row there would move what's on screen.
        .children(s.loading_older(id).then(|| row().key("older-loading").pin(Some(0.0), Some(top + 12.0), Some(0.0), None).center()
            .child(activity_ring(p))))
}
