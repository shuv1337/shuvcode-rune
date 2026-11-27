//! Tearing a tab out into a window of its own. Desktop's strip keeps a drag inside it, so this
//! is the browser's gesture, carried out in Kit's motion: pull a tab off the strip and a window
//! opens with it under the pointer, the tab where the pointer holds it, and follows the pointer
//! while the button stays down. Bring it back to the strip and the window goes; let go over
//! another window's strip and the tab moves into that strip, into the gap it opened; let go over
//! another window anywhere else and the tab joins the end of its strip; let go on the desktop
//! and the new window stays, in front. The window opens inactive, so the strip that holds the
//! press keeps it to the end.
//!
//! A window's only tab tears out too while another window is open, so a torn-out tab can go
//! back: the window it leaves empty closes, and let go on the desktop, the window moves there
//! instead.

use rune::{AppHandle, Tx, WindowHandle};
use rune_kit::shell::{TabTearMove, TabTearOut};

use super::open::{Seed, open};
use super::store::{self, Frame};
use super::{OPEN, Window};
use crate::App;
use crate::tabs::Tab;

/// A tab on its way out of this window.
#[derive(Default)]
pub enum Tear {
    #[default]
    Idle,
    Out(Out),
}

pub struct Out {
    tab: Tab,
    /// The new window's id and handle.
    id: String,
    handle: WindowHandle,
    /// The pointer from the new window's top-left, in desktop pixels.
    offset: (i32, i32),
    /// The window whose strip is under the pointer, where letting go would put the tab: its
    /// strip opens a gap there, and the tab's own window hides meanwhile.
    over: Option<Over>,
    /// The window's only tab: letting go on the desktop moves the window rather than splitting it.
    lone: bool,
}

/// Another window's strip with a gap open for the torn tab, and where the pointer last was on it.
struct Over {
    id: String,
    app: AppHandle<App>,
    at: [f32; 2],
}

/// Another window under a desktop point: its id, handle and app, and the point in its window
/// points when it's over the strip (`None` over the rest of the window).
struct Target {
    id: String,
    handle: WindowHandle,
    app: AppHandle<App>,
    at: Option<[f32; 2]>,
}

impl App {
    /// The strip's tear-out gesture, on each move while the button is down.
    pub(crate) fn tear_move(&mut self, m: TabTearMove, tx: &mut Tx) {
        let Some(point) = m.screen_point else { return };
        match (matches!(self.window.tear, Tear::Out(_)), m.torn) {
            (false, true) => self.tear_open(&m, point),
            (true, true) => self.tear_follow(point),
            (true, false) => self.tear_back(),
            (false, false) => {}
        }
        tx.live_only();
    }

    /// The torn tab's window under the pointer; hidden while the pointer is over another
    /// window's strip, where letting go would put the tab and that strip opens a gap for it.
    fn tear_follow(&mut self, point: (i32, i32)) {
        let me = self.window.id.clone();
        let Tear::Out(out) = &mut self.window.tear else { return };
        out.handle.set_position((point.0 - out.offset.0, point.1 - out.offset.1));
        // Over another window's body the tab's window stays in sight: no gap shows where it goes.
        let target = target(point, &[&me, &out.id]).and_then(|t| Some((t.at?, t)));
        if target.is_some() != out.over.is_some() {
            if target.is_some() { out.handle.hide() } else { out.handle.show() }
        }
        if out.over.as_ref().is_some_and(|o| target.as_ref().is_none_or(|(_, t)| t.id != o.id)) {
            close_gap(out.over.take());
        }
        let Some((at, t)) = target else { return };
        // The gap follows the pointer along the strip; a move within a point changes nothing.
        if out.over.as_ref().is_some_and(|o| (o.at[0] - at[0]).abs() < 1.0 && (o.at[1] - at[1]).abs() < 1.0) {
            return;
        }
        t.app.update_if(move |a, tx| a.tab_incoming(Some(at), tx));
        out.over = Some(Over { id: t.id, app: t.app, at });
    }

    /// The button came up with the tab off the strip.
    pub(crate) fn tear_release(&mut self, t: TabTearOut, tx: &mut Tx) {
        // Where it let go is unknown: the tab stays here, and a window following it goes.
        let Some(point) = t.screen_point else { return self.tear_back() };
        let out = match std::mem::take(&mut self.window.tear) {
            Tear::Out(out) => out,
            // No window followed the pointer: open one where it let go.
            Tear::Idle => {
                let Some(tab) = self.tab(&t.id).cloned() else { return };
                if let Some(target) = target(point, &[&self.window.id]) {
                    return self.move_tab_to(tab, target, tx);
                }
                // The only tab, let go on the desktop: its window is already its own.
                if self.tabs.len() < 2 {
                    return;
                }
                let Some((frame, _)) = self.torn_frame(point, [0.0, 0.0]) else { return };
                let seed = Seed::from(self, vec![tab.clone()], tab.route());
                if open(self, seeded(seed), Some(frame), false).is_some() {
                    self.tab_moved_out(&tab.uid(), tx);
                }
                return;
            }
        };
        match target(point, &[&self.window.id, &out.id]) {
            Some(target) => {
                out.handle.close();
                if out.over.as_ref().is_some_and(|o| o.id != target.id) {
                    close_gap(out.over);
                }
                self.move_tab_to(out.tab, target, tx);
            }
            // The only tab: its window goes where the tab's is, as a browser's moves with it.
            None if out.lone => {
                close_gap(out.over);
                if let (Some(p), Some(me)) = (out.handle.placement(), super::handle(&self.window.id)) {
                    me.set_position(p.position);
                    me.focus();
                }
                out.handle.close();
            }
            None => {
                close_gap(out.over);
                super::life(&out.id, super::life::LifeEvent::Kept);
                out.handle.show();
                out.handle.focus();
                self.tab_moved_out(&out.tab.uid(), tx);
            }
        }
    }

    /// `poll`: a gesture that ended without a release (Escape, the window losing focus) takes
    /// its window back.
    pub(crate) fn poll_tear(&mut self) {
        if matches!(self.window.tear, Tear::Out(_)) && !self.gestures.tab_drag.torn_out() && !crate::tabs::groups::torn_out(self) {
            self.tear_back();
        }
    }

    /// The tab is back on the strip: its window goes, and a gap it opened in another closes.
    pub(crate) fn tear_back(&mut self) {
        if let Tear::Out(out) = std::mem::take(&mut self.window.tear) {
            out.handle.close();
            close_gap(out.over);
        }
    }

    /// Pulled off the strip: a window opens with the tab, under the pointer, inactive, as wide
    /// and tall as this one.
    fn tear_open(&mut self, m: &TabTearMove, point: (i32, i32)) {
        if !self.can_tear() {
            return;
        }
        let lone = self.tabs.len() == 1;
        let Some(tab) = self.tab(&m.id).cloned() else { return };
        let Some((frame, offset)) = self.torn_frame(point, m.grab) else { return };
        let seed = Seed::from(self, vec![tab.clone()], tab.route());
        let mut w = seeded(seed);
        w.life = super::life::Life::Torn;
        let id = w.id.clone();
        let Some(handle) = open(self, w, Some(frame), true) else { return };
        self.gestures.tab_drag.hand_off();
        self.window.tear = Tear::Out(Out { tab, id, handle, offset, over: None, lone });
    }

    /// Where a window showing the torn tab goes so the pointer at `point` holds it at `grab`
    /// (points in the tab) as it held it here: the tab leads the new window's strip. And the
    /// pointer's offset from the window's top-left.
    fn torn_frame(&self, point: (i32, i32), grab: [f32; 2]) -> Option<(Frame, (i32, i32))> {
        let p = super::handle(&self.window.id)?.placement()?;
        let strip = self.window.strip.unwrap_or([0.0; 4]);
        let scale = p.scale as f32;
        let inset = (p.content_position.0 - p.position.0, p.content_position.1 - p.position.1);
        let x = ((strip[0] + grab[0] * *self.zoom) * scale).round() as i32 + inset.0;
        let y = ((strip[1] + grab[1] * *self.zoom) * scale).round() as i32 + inset.1;
        let frame = Frame { x: Some(point.0 - x), y: Some(point.1 - y), width: p.content_size.0, height: p.content_size.1, ..Frame::default() };
        Some((frame, (x, y)))
    }

    /// Whether a tab can leave the strip for a window of its own or another window's strip:
    /// one of several, or the only one while another window is open to take it.
    pub(crate) fn can_tear(&self) -> bool {
        self.persists() && (self.tabs.len() > 1 || (self.tabs.len() == 1 && self.other_windows()))
    }

    /// Sends `tab` to the window under the pointer, where it joins the strip, and drops it here.
    fn move_tab_to(&mut self, tab: Tab, target: Target, tx: &mut Tx) {
        let draft = (self.draft_text(&tab.id), self.draft_parked(&tab.id));
        let id = tab.uid();
        let at = target.at;
        target.app.update(move |a, tx| a.receive_tab(tab, draft, at, tx));
        target.handle.focus();
        self.tab_left(&id, tx);
    }

    /// Tab `id` went to another window; a window it leaves with no tabs closes.
    pub(crate) fn tab_left(&mut self, id: &str, tx: &mut Tx) {
        self.tab_moved_out(id, tx);
        if self.tabs.is_empty() {
            tx.close_window();
        }
    }

    /// A tab dragged from another window is over this window's strip at `at` (window points), or
    /// left it: the strip opens a gap where it would land (`TabDragState::set_incoming`).
    pub(crate) fn tab_incoming(&mut self, at: Option<[f32; 2]>, tx: &mut Tx) -> bool {
        let open = self.gestures.tab_drag.incoming_slot().is_some();
        // The strip lays out in the view's points, the window's over its zoom.
        self.gestures.tab_drag.set_incoming(at.map(|a| a.map(|v| v / *self.zoom)));
        if !open && at.is_none() {
            tx.live_only();
            return false;
        }
        true
    }

    /// A tab dropped onto this window from another: on the strip at `at` (window points), into
    /// the gap the strip opened for it, found from its own tabs, or where `at` falls when the strip
    /// never opened one (a drop with no window following the pointer); anywhere else, at the end.
    pub(crate) fn receive_tab(&mut self, tab: Tab, draft: (Option<String>, Option<crate::restore::Parked>), at: Option<[f32; 2]>, tx: &mut Tx) {
        let (text, parked) = draft;
        if let Some(text) = text {
            self.kernel.slots.get_mut(&crate::restore::RESTORE).drafts.insert(tab.id.clone(), text);
        }
        if let Some(parked) = parked {
            self.kernel.slots.get_mut(&crate::restore::RESTORE).parked.insert(tab.id.clone(), parked);
        }
        let slot = self.gestures.tab_drag.incoming_slot();
        self.gestures.tab_drag.set_incoming(None);
        if !self.tabs.iter().any(|t| t.uid() == tab.uid()) {
            let index = slot.or_else(|| at.map(|at| self.drop_index(at))).unwrap_or(self.tabs.len()).min(self.tabs.len());
            self.tabs.insert(index, tab.clone());
        }
        self.select_tab(&tab.uid(), tx);
        self.save_tabs();
    }

    /// The strip place a point lands on, every tab taken as long as the strip's longest: the
    /// slot whose middle it passed.
    fn drop_index(&self, at: [f32; 2]) -> usize {
        let n = self.tabs.len();
        let Some(r) = self.window.strip.filter(|_| n > 0) else { return n };
        let vertical = self.tab_layout().has_sidebar();
        let (from, span, most) = if vertical { (r[1], r[3], 31.0) } else { (r[0], r[2], 230.0) };
        let pitch = (span / n as f32).min(most * *self.zoom).max(1.0);
        let along = if vertical { at[1] } else { at[0] };
        (((along - from) / pitch).round().max(0.0) as usize).min(n)
    }

    /// Tab `key` (`App::tab_at`) went to another window: it leaves this strip without a closed-tab entry, and its
    /// words go with it.
    fn tab_moved_out(&mut self, key: &str, tx: &mut Tx) {
        let Some(i) = self.tab_at(key) else { return };
        let shown = self.current_tab() == Some(self.tabs[i].uid());
        // Collect and close browser tabs owned by the moved-out session (and its non-tabbed
        // subagents) BEFORE `self.tabs.remove(i)` so `panel_root` resolves subagent strips cleanly
        // and without polluting `ClosedSideTab` (`⇧⌘T`) history.
        let id = self.tabs[i].id.clone();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let moved_sids: Vec<String> = self
                .workspace_panes
                .browser
                .session_tabs
                .keys()
                .filter(|sid| {
                    sid.as_str() == id.as_str()
                        || (crate::launch::saved::page_tab::root_of(self, sid) == id.as_str()
                            && !self.tabs.iter().any(|t| !t.is_draft() && t.id == **sid))
                })
                .cloned()
                .collect();
            for sid in moved_sids {
                self.close_browser_tabs_for_session(&sid);
            }
        }
        let id = self.tabs.remove(i).id;
        let r = self.kernel.slots.get_mut(&crate::restore::RESTORE);
        r.drafts.remove(&id);
        r.parked.remove(&id);
        self.kernel.slots.get_mut(&crate::add_menu::ADD_MENU).park(id, session_view::composer::mode::Mode::Normal);
        if shown {
            self.drop_shown_draft();
            match self.tabs.get(i).or_else(|| self.tabs.get(i.wrapping_sub(1))).map(Tab::uid) {
                Some(next) => self.select_tab(&next, tx),
                None => self.go_home(tx),
            }
        }
        self.save_tabs();
    }
}

/// The strip `over` opened a gap in closes it: the tab went elsewhere.
fn close_gap(over: Option<Over>) {
    if let Some(o) = over {
        o.app.update_if(|a, tx| a.tab_incoming(None, tx));
    }
}

/// A new window holding what `seed` moves.
fn seeded(seed: Seed) -> Window {
    let mut w = Window::new(store::new_id(), false);
    w.seed = Some(seed);
    w
}

type OpenWindow = (String, WindowHandle, AppHandle<App>, Option<[f32; 4]>, u64);

/// The front-most window, other than `skip`, under desktop point `point` (`at` set when the
/// point is on its strip).
fn target(point: (i32, i32), skip: &[&str]) -> Option<Target> {
    let mut open: Vec<OpenWindow> = OPEN.with(|o| {
        o.borrow().iter().filter(|w| w.life.saved() && !skip.contains(&w.id.as_str())).map(|w| (w.id.clone(), w.handle.clone(), w.app.clone(), w.strip, w.focused)).collect()
    });
    open.sort_by_key(|w| std::cmp::Reverse(w.4));
    open.into_iter().find_map(|(id, handle, app, strip, _)| {
        let p = handle.placement().filter(|p| !p.minimized)?;
        let s = p.scale as f32;
        let at = [(point.0 - p.content_position.0) as f32 / s, (point.1 - p.content_position.1) as f32 / s];
        let size = [p.content_size.0 as f32 / s, p.content_size.1 as f32 / s];
        let (on_strip, inside) = hit(at, strip, size);
        inside.then(|| Target { id, handle, app, at: on_strip.then_some(at) })
    })
}

/// A point in a window's points: whether it's on the strip `strip` (with a little slack, as the
/// strip's own reattach distance gives), and whether it's in the window at all (`size`).
pub(crate) fn hit(at: [f32; 2], strip: Option<[f32; 4]>, size: [f32; 2]) -> (bool, bool) {
    let slack = 12.0;
    let on_strip = strip.is_some_and(|r| at[0] >= r[0] - slack && at[0] <= r[0] + r[2] + slack && at[1] >= r[1] - slack && at[1] <= r[1] + r[3] + slack);
    let inside = on_strip || (at[0] >= 0.0 && at[1] >= 0.0 && at[0] <= size[0] && at[1] <= size[1]);
    (on_strip, inside)
}
