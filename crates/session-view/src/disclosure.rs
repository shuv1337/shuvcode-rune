//! Opening and closing a disclosure: Desktop's `DisclosureState` (`session-ui/src/components/
//! disclosure-machine.ts:23-36`) for every disclosure in the timeline, keyed by its key.
//!
//! A disclosure the reader never toggled isn't listed and follows its default (Desktop's
//! `touched: false`: `props.open ?? defaultOpen`, `disclosure-machine.ts:135`). Toggling one
//! makes it the reader's: [`Disclosure::Open`] or [`Disclosure::Shut`]. A fold (a tool group, a
//! Compacted summary) that shuts keeps what it showed laid out under its row's clip while the row
//! springs shut ([`Disclosure::Shutting`]), then follows its default (shut) again. Desktop's
//! deferred body mounts (`DisclosureMount`) have no counterpart: Rune builds a body in the frame
//! it opens.

use std::collections::HashMap;

use rune::machine::Effects;
use rune::{Edge, El, Reveal, Spring, SpringPair, Transition, Tx};
use rune_knobs::{Group, Knob, Knobs};

use crate::TimelineHost;

/// A tool group folding open or shut, as a critically damped spring's duration: Kit asked for
/// 150-200 ms, ease out, no bounce. However long the group, only what shows of it moves.
pub const FOLD_SPEED: Knob = Knob::new("Fold speed", 0.08, 0.5, 0.18).step(0.01).about("Seconds a tool group takes to fold");

/// How deep a fold's bottom edge feathers while it moves, opening and shutting: soft enough
/// that lines dissolve into the rows below rather than being cut, crisp again once it lands.
pub const FOLD_FEATHER: Knob = Knob::new("Fold edge", 0.0, 64.0, 24.0).step(1.0).about("Points the moving edge fades over");

pub const GROUP: Group = Group::new("Tool groups", "tool-groups", &[FOLD_SPEED, FOLD_FEATHER]).under("Timeline");

/// One disclosure the reader toggled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Disclosure {
    /// `status: "open"`, touched.
    Open,
    /// `status: "closed"`, touched: shut where its default shows it open.
    Shut,
    /// A fold springing shut: what it showed stays mounted under the row's clip until `until`
    /// on the app clock, then it follows its default (shut) again.
    Shutting { until: f64 },
}

/// `toggle.requested` and the fold's own timer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// The reader opened it.
    Open,
    /// The reader shut it; `default_open` is what it shows untouched.
    Shut { default_open: bool },
    /// A fold shuts: it springs shut until `until`.
    Fold { until: f64 },
    /// It follows its default again (a story or test resetting it).
    Forget,
    /// The app clock reached `now`: folds that finished shutting drop.
    Tick { now: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Effect {
    /// A fold's body unmounts at this time on the app clock.
    WakeAt(f64),
}

/// Every disclosure the reader toggled, by key; one not listed follows its default.
#[derive(Clone, Debug, Default)]
pub struct Disclosures(HashMap<String, Disclosure>);

impl Disclosures {
    /// One transition of disclosure `key`.
    pub fn step(&mut self, key: &str, event: Event) -> Effects<Effect> {
        match event {
            Event::Open => {
                self.0.insert(key.to_owned(), Disclosure::Open);
                Effects::none()
            }
            Event::Shut { default_open: true } => {
                self.0.insert(key.to_owned(), Disclosure::Shut);
                Effects::none()
            }
            Event::Shut { default_open: false } | Event::Forget => {
                self.0.remove(key);
                Effects::none()
            }
            Event::Fold { until } => {
                self.0.insert(key.to_owned(), Disclosure::Shutting { until });
                Effects::one(Effect::WakeAt(until))
            }
            Event::Tick { now } => {
                self.0.retain(|_, d| !matches!(d, Disclosure::Shutting { until } if *until <= now + 1e-9));
                self.next_due().map_or(Effects::none(), |due| Effects::one(Effect::WakeAt(due)))
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<Disclosure> {
        self.0.get(key).copied()
    }

    /// Whether the reader opened `key`.
    pub fn contains(&self, key: &str) -> bool {
        self.get(key) == Some(Disclosure::Open)
    }

    /// What the reader toggled among the keys that start with `prefix`, as a number that moves
    /// when one of them opens, shuts or follows its default again: for a memo whose row draws
    /// such a disclosure (`rune::memo` builds again only when its version moves).
    pub fn stamp(&self, prefix: &str) -> u64 {
        use std::hash::{DefaultHasher, Hash, Hasher};
        // The map has no order, so the sum of each one's hash.
        self.0.iter().filter(|(key, _)| key.starts_with(prefix)).fold(0, |sum, (key, state)| {
            let mut h = DefaultHasher::new();
            (key, std::mem::discriminant(state)).hash(&mut h);
            sum.wrapping_add(h.finish())
        })
    }

    /// Opens `key`; true when it wasn't open.
    pub fn insert(&mut self, key: String) -> bool {
        let was = self.contains(&key);
        self.step(&key, Event::Open);
        !was
    }

    /// `key` follows its default again; true when it was open.
    pub fn remove(&mut self, key: &str) -> bool {
        let was = self.contains(key);
        self.step(key, Event::Forget);
        was
    }

    /// The keys the reader opened.
    pub fn iter(&self) -> impl Iterator<Item = &String> {
        self.0.iter().filter(|(_, d)| **d == Disclosure::Open).map(|(k, _)| k)
    }

    /// How many the reader opened.
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every disclosure the reader opened (`true`) or shut (`false`), for keeping across a
    /// restart; one springing shut counts as shut.
    pub fn touched(&self) -> impl Iterator<Item = (&String, bool)> {
        self.0.iter().map(|(k, d)| (k, *d == Disclosure::Open))
    }

    /// Takes back what [`Disclosures::touched`] kept; what the reader toggled since stays.
    pub fn restore(&mut self, touched: impl IntoIterator<Item = (String, bool)>) {
        for (k, open) in touched {
            self.0.entry(k).or_insert(if open { Disclosure::Open } else { Disclosure::Shut });
        }
    }

    /// Keeps the toggled disclosures whose key passes `keep`.
    pub fn retain(&mut self, mut keep: impl FnMut(&String) -> bool) {
        self.0.retain(|k, _| keep(k));
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// When the next fold finishes shutting.
    fn next_due(&self) -> Option<f64> {
        self.0.values().filter_map(|d| if let Disclosure::Shutting { until } = d { Some(*until) } else { None }).reduce(f64::min)
    }
}

impl Extend<String> for Disclosures {
    fn extend<I: IntoIterator<Item = String>>(&mut self, keys: I) {
        for k in keys {
            self.insert(k);
        }
    }
}

/// The tool group row's own spring (`El::fold_glide`).
pub fn fold(k: &Knobs) -> Spring {
    Spring::new(k.get(&FOLD_SPEED), 0.0)
}

/// What a disclosure shows open (a tool's output, a thought), on the fold's spring: it wipes in
/// from its top while the rows below slide down on the same spring ([`set`], [`toggle`]), and
/// shutting, wipes back up as its slot closes. `content` needs a key that holds from build to
/// build.
pub fn reveal(k: &Knobs, content: El) -> El {
    let spring = fold(k);
    let mut t = Transition::reveal(Edge::Top).springs(SpringPair::new(spring, spring)).hold_layout();
    let wipe = Some(Reveal { edge: Edge::Top, feather: k.get(&FOLD_FEATHER) });
    (t.insertion.reveal, t.removal.reveal) = (wipe, wipe);
    content.transition(t)
}

/// How long a shut tool group keeps its steps mounted, for the row to fold shut over them:
/// twice the spring's duration, when under a twentieth of a point is left to go.
pub fn shut_secs(k: &Knobs) -> f64 {
    2.0 * crate::knobs::secs(k, &FOLD_SPEED)
}

/// Opens `key` if the reader hadn't, else lets it follow its default (shut); true when it is now
/// open.
pub fn toggle_expanded<S: TimelineHost>(s: &mut S, key: &str) -> bool {
    let opening = !s.expanded().contains(key);
    s.expanded_mut().step(key, if opening { Event::Open } else { Event::Forget });
    opening
}

/// Whether disclosure `key` is open: as last opened or shut, else `default` (Timeline detail's
/// Collapse setting for shells, edits and thinking, as Desktop's `disclosure.value(id) ?? default`).
pub fn is_open<S: TimelineHost>(s: &S, key: &str, default: bool) -> bool {
    match s.expanded().get(key) {
        Some(d) => d == Disclosure::Open,
        None => default,
    }
}

/// Opens or shuts `key` whatever its default.
pub fn set_open<S: TimelineHost>(s: &mut S, key: &str, open: bool) {
    s.expanded_mut().step(key, if open { Event::Open } else { Event::Shut { default_open: true } });
}

/// A press handler that opens or shuts `key`, as [`toggle`] flips it, for a disclosure with a
/// default (`open`: whether it shows open now).
pub fn set<S: TimelineHost>(key: String, open: bool, hold: String) -> impl Fn(&mut S, &mut Tx) + 'static {
    move |s: &mut S, tx: &mut Tx| {
        tx.motion(fold(s.knobs()));
        set_open(s, &key, !open);
        anchor(s, tx, !open, &hold, &hold);
    }
}

/// A press handler that opens or shuts `key` without layout or growth motion, keeping `hold` in
/// place when scrolled up: for a file diff (`packages/session-ui/src/timeline-next/tools/patch.tsx`
/// `FileAccordionItem`, `packages/ui/src/data-display/accordion/accordion.css`), whose content
/// mounts and unmounts directly under its header instead of re-running its parent group's fold
/// glide.
pub fn set_instant<S: TimelineHost>(key: String, open: bool, hold: String) -> impl Fn(&mut S, &mut Tx) + 'static {
    move |s: &mut S, tx: &mut Tx| {
        tx.instant();
        set_open(s, &key, !open);
        anchor(s, tx, !open, &hold, &hold);
    }
}

/// Steps the open session's follow state with a disclosure opening or shutting, `row` the
/// opened row and `header` the pressed one, and makes the hold it asks for: none at the
/// timeline's end, where the end holds; scrolled up, the row or header (`follow::step`).
pub fn anchor<S: TimelineHost>(s: &mut S, tx: &mut Tx, opening: bool, row: &str, header: &str) {
    // Disclosure is a no-op in Following (follow::step). Taking the mutable Tracked
    // Ends anyway invalidates every region that read the scroll position.
    if s.active_session_id().is_some_and(|id| s.ends().at_end(id)) { return; }
    let id = s.active_session_id().map(str::to_owned);
    if let Some(crate::follow::Effect::Keep(key)) = s.ends_mut().step(id.as_deref(), crate::follow::Event::Disclosure { opening, row, header }) {
        tx.keep_on_screen(key);
    }
}

/// Whether fold `key` (a tool group, a Compacted summary) is springing shut, what it showed
/// still laid out under the clip.
pub fn is_shutting<S: TimelineHost>(s: &S, key: &str) -> bool {
    matches!(s.expanded().get(key), Some(Disclosure::Shutting { until }) if s.clock() < until)
}

/// A press handler that flips `key`: what it shows folds open or shut ([`reveal`]), the rows
/// below moving with it on the fold's spring. `hold`, the pressed trigger, holds as [`anchor`]
/// says.
pub fn toggle<S: TimelineHost>(key: String, hold: String) -> impl Fn(&mut S, &mut Tx) + 'static {
    move |s: &mut S, tx: &mut Tx| {
        tx.motion(fold(s.knobs()));
        let opening = toggle_expanded(s, &key);
        anchor(s, tx, opening, &hold, &hold);
    }
}

/// Flips fold `key`, a timeline row that folds on its own spring (`RowSpec::wrap`); true when it
/// is now open. Shutting, it keeps what it shows mounted until `poll` drops it, so the row's
/// clip folds it away, and a press mid-motion springs back from there.
pub fn flip_fold<S: TimelineHost>(s: &mut S, key: &str) -> bool {
    let open = !s.expanded().contains(key);
    let event = if open { Event::Open } else { Event::Fold { until: s.clock() + shut_secs(s.knobs()) } };
    // The fold's own wake comes from `poll`, which runs after every event.
    let _ = s.expanded_mut().step(key, event);
    open
}

/// Opens or shuts tool group `key` (`turns::group`), the row folding open or shut on its own
/// spring. The timeline holds as [`anchor`] says: at its end, the end stays and the header rides
/// up as the steps open; scrolled up, the group's row keeps its top and the steps push the rows
/// below down. One pressed half under the top edge keeps its row's top there, and the header,
/// sticky once open, stops at the edge with its steps under it (p277).
pub fn toggle_group<S: TimelineHost>(key: String) -> impl Fn(&mut S, &mut Tx) + 'static {
    move |s: &mut S, tx: &mut Tx| {
        let opening = flip_fold(s, &key);
        anchor(s, tx, opening, &key, &format!("{key}/trigger"));
    }
}

/// Collapses tool group `key` only if it is currently open; clicking a nested rail while the row
/// is already folding shut is a no-op instead of re-opening it (`nested-rail.ts`).
pub fn collapse_group<S: TimelineHost>(key: String) -> impl Fn(&mut S, &mut Tx) + 'static {
    move |s: &mut S, tx: &mut Tx| {
        if !s.expanded().contains(&key) {
            return;
        }
        let opening = flip_fold(s, &key);
        anchor(s, tx, opening, &key, &format!("{key}/trigger"));
    }
}

/// Drops the steps of groups that have finished shutting, and wakes for the next.
pub fn poll<S: TimelineHost>(s: &mut S, now: f64, tx: &mut Tx) -> bool {
    // Every poll comes here: the disclosures are written only when one finished shutting.
    if !s.expanded().0.values().any(|d| matches!(d, Disclosure::Shutting { until } if *until <= now + 1e-9)) {
        if let Some(due) = s.expanded().next_due() {
            tx.wake_at(due);
        }
        return false;
    }
    let before = s.expanded().0.len();
    for effect in s.expanded_mut().step("", Event::Tick { now }) {
        let Effect::WakeAt(due) = effect;
        tx.wake_at(due);
    }
    s.expanded().0.len() != before
}
