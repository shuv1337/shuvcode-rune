use rune::Shadow;
use rune_kit::shell::TabOrientation;
use rune_knobs::{Group, Knob};

use super::*;
use crate::layout_state::{PeekEvent, SidebarPeekState};
use opencode_i18n::t;

mod auto_sections;
mod customized;
mod menu;
pub(crate) mod grouping;
pub(crate) mod pinned_area;
pub(crate) mod recent;
pub(crate) mod recent_view;
pub(crate) mod pins;
pub(super) mod rows;
mod sections;
mod tab_groups;
pub(crate) mod state;

// ---------------------------------------------------------------------------------------------
// Sidebar: Desktop's vertical tabs (`shell.tsx`, `titlebar.tsx`, `tab-strip.tsx`). Top tabs have
// none: the aside narrows to nothing and its tabs fly to the titlebar.

/// Default, minimum and maximum widths (`shell.tsx`'s `tabsWidth` and its ResizeHandle).
pub const SIDEBAR_W: f32 = 240.0;
pub const SIDEBAR_MIN: f32 = 136.0;
pub const SIDEBAR_MAX: f32 = 640.0;
/// The sidebar's start and end padding (`--sidebar-inset-start`, `--sidebar-inset-end`).
pub(super) const INSET_START: f32 = 11.0;
pub(super) const INSET_END: f32 = 3.0;
/// The sidebars' shared spring, "smooth" (`panel-motion.ts`).
pub(super) const SIDEBAR_SPRING: Spring = Spring::new(0.3, 0.0);
/// Under Reduce Motion a peek fades in place (`0.15s`).
const PEEK_FADE: Spring = Spring::new(0.15, 0.0);
/// How far a closed panel's contents dim at the end of the flight.
pub(super) const FADE_FLOOR: f32 = 0.5;
/// The peek's edge zone starts this far below the sidebar's top, clear of the traffic lights
/// and the toggle (`sidebar-peek.ts`); the corridor reaches this far past the sidebar.
const EDGE_TOP: f32 = 56.0;
const CORRIDOR: f32 = 15.0;

/// How far in from the window's start the pointer must be for the sidebar to peek. Desktop's
/// 24 (its 8pt gutter and 16 more) caught passes to the panel's first column.
pub const PEEK_EDGE: Knob = Knob::new("Peek edge", 2.0, 24.0, 6.0).step(1.0).about("How close to the edge it opens");
/// How long the pointer rests there first (Desktop's 40 ms).
pub const PEEK_DELAY: Knob = Knob::new("Peek delay", 0.0, 0.4, 0.08).step(0.01).about("Rest at the edge before it opens");
/// The slide in's and the slide out's springs, with no bounce (Desktop's 0.18 s both ways).
pub const PEEK_IN: Knob = Knob::new("Peek in", 0.08, 0.4, 0.18).step(0.01).about("Slide-in spring duration");
pub const PEEK_OUT: Knob = Knob::new("Peek out", 0.06, 0.4, 0.13).step(0.01).about("Slide-out spring duration");
/// The shadow along the peek's end edge: its strength, and the soft layer's blur.
pub const PEEK_SHADOW: Knob = Knob::new("Peek shadow", 0.0, 3.0, 1.8).step(0.05).about("Strength of the edge shadow");
pub const PEEK_SHADOW_BLUR: Knob = Knob::new("Peek shadow blur", 0.0, 64.0, 28.0).step(1.0).about("How far the soft shadow spreads");
pub const PEEK_KNOBS: Group = Group::new("Sidebar peek", "window", &[PEEK_EDGE, PEEK_DELAY, PEEK_IN, PEEK_OUT, PEEK_SHADOW, PEEK_SHADOW_BLUR]);

rune_register::register!(crate::knobs::GROUP_LIST: Group = [PEEK_KNOBS]);

/// Home, Search sessions and New session (`data-titlebar-tab-action`): 28pt rows, `px-2 gap-2`,
/// faint until hovered or pressed, their keys as keycap chips at the row's end on hover or
/// keyboard focus (`titlebar.tsx` SidebarAction, `recently-closed-tabs-menu.tsx`).
pub(super) fn action_row(p: &Pal, key: &str, glyph: &'static Icon, label: &str, keys: Vec<String>, pressed: bool) -> El {
    let ink = if pressed { p.text } else { p.faint };
    // `text-[13px] leading-[var(--line-height-compact)]` (no negative tracking), `min-w-0 truncate`.
    let name = || text(label).font(TextStyle::new(13.0, 440.0).lh(16.0)).color(ink).group_color(p.text).truncate();
    let r = row()
        .key(key)
        .role("button")
        .label(label)
        .h(28.0)
        .w_full()
        .px(8.0)
        .gap(8.0)
        .center_y()
        .radius(6.0)
        .focus_inset(p)
        .hover_group_focus()
        .hover(if pressed { Color::TRANSPARENT } else { p.hover })
        .press(if pressed { Color::TRANSPARENT } else { p.pressed })
        .hover_motion(INSTANT, INSTANT)
        .child(icon(glyph, 16.0).color(ink).group_color(p.text));
    let r = if pressed { r.fill(p.pressed) } else { r };
    if keys.is_empty() {
        return r.child(name());
    }
    // `ms-auto hidden shrink-0 group-hover:block group-focus-visible:block`: the keys take room
    // only while they show, and the label shortens for them. A group's styles paint and lay
    // nothing out, so the row holds both layouts and shows one.
    let whole = Some(0.0);
    r.child(
        el().key("name")
            .grow(1.0)
            .min_w(0.0)
            .h(28.0)
            .child(row().key("rest").pin(whole, whole, whole, whole).center_y().child(name()).group_opacity(1.0, 0.0))
            .child(
                // Desktop's keys never shrink, so in a sidebar too narrow for them they run past
                // the row's end. Here they wrap out of sight instead and the label keeps the row.
                row().key("keyed")
                    .pin(whole, whole, whole, whole)
                    .flex_wrap()
                    .gap(8.0)
                    .clip_soft(0.0)
                    .a11y_hidden()
                    .group_opacity(0.0, 1.0)
                    .child(row().grow(1.0).min_w(0.0).h(28.0).center_y().child(name()))
                    .child(row().key("hint").h(28.0).center_y().child(crate::keymap::chips(p, keys))),
            ),
    )
}

/// The docked sidebar: its aside's width moves the panel's edge while its contents keep their
/// width, pinned to that edge. Top tabs keep the aside at no width, empty.
///
/// The hover peek is the same contents, slid out over the panel while the aside stays shut
/// (`sidebar-peek.css`): one layer that mounts nothing, so no row flies on its own, and docking
/// from a peek only widens the aside under it.
pub(crate) fn sidebar(s: &App, p: &Pal) -> El {
    let switching = s.switching_layout();
    // The aside reaches into the gap before the panel, taking it back with its end margin so the
    // panel keeps its place: scroll thumbs can run past the sidebar's end inside the aside's clip.
    let aside = el().key("sidebar").role("navigation").h_full().mr(-GAP_ROOM).motion(if switching { GLIDE } else { SIDEBAR_SPRING });
    if !s.shows_sidebar() {
        return aside.w(GAP_ROOM).clip_soft(0.0);
    }
    let (w, open, peek, k) = (*s.sidebar_w, *s.sidebar_open, s.layout.peek, &s.diag.knobs);
    let reduced = *s.appearance_state.reduced_motion;
    // Past the window's start: closed, or a peek sliding away. Under Reduce Motion a peek fades
    // where it is instead.
    let away = !open && !peek.open();
    // Away past the window's start edge: its left, or its right in a right-to-left window (an
    // offset is physical, so it turns with the direction here).
    let rtl = opencode_i18n::direction() == opencode_i18n::Direction::Rtl;
    let surface = surface_width(s);
    let x = if away && !(reduced && peek.shown()) { if rtl { surface } else { -surface } } else { 0.0 };
    let slide = match peek {
        SidebarPeekState::Leaving { .. } => Spring::new(k.get(&PEEK_OUT), 0.0),
        _ if peek.shown() => Spring::new(k.get(&PEEK_IN), 0.0),
        _ => SIDEBAR_SPRING,
    };
    // A peek is solid from its first frame: it turns opaque while still out of sight.
    let (opacity, fade): (f32, rune::Anim) = match () {
        _ if peek.shown() && reduced => (if away { 0.0 } else { 1.0 }, PEEK_FADE.into()),
        _ if peek.shown() => (1.0, INSTANT),
        _ if open => (1.0, SIDEBAR_SPRING.into()),
        _ => (if reduced { 0.0 } else { FADE_FLOOR }, SIDEBAR_SPRING.into()),
    };
    let row_h = chrome().row_h;
    let mut inner = contents(s, p, chrome().top() - row_h)
        .key("sidebar-inner")
        .pin(Some(0.0), Some(row_h), None, Some(0.0))
        .fill(p.bg_deep)
        .offset(x, 0.0)
        .offset_motion(slide)
        .opacity(opacity)
        .opacity_motion(fade);
    // Lifted off the panel along its end edge while it's out; the shadow fades as it slides
    // away or docks, so none lingers along the window's edge.
    let lift = if peek.open() { k.get(&PEEK_SHADOW) } else { 0.0 };
    let tint = |c: Color| c.alpha((c.a * lift).min(1.0));
    let blur = k.get(&PEEK_SHADOW_BLUR);
    inner = inner
        // The shadow falls over the chat, on the side away from the window's edge.
        .box_shadow(Shadow { x: if rtl { -blur / 3.0 } else { blur / 3.0 }, blur, color: tint(p.shadow), ..Shadow::default() })
        .box_shadow(Shadow { x: if rtl { -1.0 } else { 1.0 }, blur: 4.0, color: tint(p.shadow), ..Shadow::default() })
        .box_shadow(Shadow { color: tint(p.floating[2].color), ..p.floating[2] });
    // Out of sight it's out of reach: no Tab stops past the window's edge, nothing a reader
    // reads as on screen (Desktop's `guardClosingFocus` hides the closed panel the same way).
    if away && !peek.shown() {
        inner = inner.inert().a11y_hidden();
    }
    let aside = aside.w(if open { w } else { 0.0 } + GAP_ROOM);
    if peek.shown() {
        // Over the panel, which comes after it; the aside's clip would hide it.
        aside.child(inner.raise())
    } else {
        aside.clip_soft(0.0).child(inner)
    }
}

/// How far the aside reaches past the sidebar into the gap before the panel (`--shell-inline-inset`).
const GAP_ROOM: f32 = 8.0;

/// A floating mount covers the panel gutter as additional end padding
/// (`shell/shell.tsx`, `sidebar-peek.css`, 9b57cc05cb). Rows stay docked in place.
fn surface_width(s: &App) -> f32 {
    *s.sidebar_w + if s.layout.peek.shown() { GAP_ROOM } else { 0.0 }
}

/// What the sidebar holds, `w` wide from `top` down: the header line where the traffic lights
/// share it, Home, Search sessions and New session, then the tabs (or the inbox).
fn contents(s: &App, p: &Pal, top: f32) -> El {
    let chrome = chrome();
    let home = s.route == Route::Home && !s.settings_open();
    let new = || action_row(p, "new", &ic::EDIT, t!("command.session.new"), crate::keymap::Bind::TabNew.keycaps(), false)
        .matched(super::titlebar::NAV_NEW).matched_crop().motion(GLIDE)
        .on_press_tx(|s: &mut App, tx: &mut Tx| s.new_session(tx))
        .on_secondary_press(|s: &mut App, press, tx| crate::recently_closed::open(s, press, tx));
    col()
        .w(surface_width(s))
        .pt(top)
        .pl(INSET_START)
        .pr(if s.layout.peek.shown() { INSET_END + GAP_ROOM } else { INSET_END })
        .pb(10.0)
        // Shares the session header's 48px centre line with the traffic lights; drags the window.
        // A titlebar row above holds the toggle instead (`titlebar.tsx` on Windows).
        .children(chrome.sidebar_header().then(|| {
            row()
                .key("sidebar-header")
                .w_full()
                .h(48.0)
                .mb(2.0)
                .gap(8.0)
                .center_y()
                .window_drag()
                // The lights' end, less the sidebar's start padding.
                .child(el().key("lights").w((chrome.lights_end - INSET_START).max(0.0)).h(28.0))
                // Linux has no titlebar row with side tabs, so its app menu leads this header
                // (official `titlebar.tsx`'s `linux()` branch, opencode#65912): its glyph sits 2
                // further into its slot than the sidebar's icons sit into their rows (`-ms-0.5`).
                .children(cfg!(target_os = "linux").then(|| crate::app_menu::button(s, p).ml(-2.0)))
                .child(spacer())
                // The toggle floats above (see `sidebar_toggle`); its slot keeps the row's shape.
                .child(el().key("toggle-slot").size(28.0, 28.0))
        }))
        .child(col().key("actions").w_full().gap(3.0).children(actions(s, p, home, new)))
        .child(if crate::tabs::groups::on(s) { tab_groups::controls(s, p) } else { el().key("tabs-gap").w_full().h(16.0) })
        .child(list(s, p))
        .children(crate::plugin_host::sidebar_items(s, p))
}

/// Home, Search sessions and New session; Customize trades Home for Settings at the end;
/// Consolidated lists New session, Sessions and Settings (`titlebar.tsx`, #64668).
fn actions(s: &App, p: &Pal, home: bool, new: impl Fn() -> El) -> Vec<El> {
    let home_row = || {
        let (label, glyph) = pinned_area::home_label(s);
        action_row(p, "home", glyph, label, crate::keymap::Bind::Home.keycaps(), home)
            .matched(super::titlebar::NAV_HOME).matched_crop().motion(GLIDE)
            .on_press_tx(|s: &mut App, tx: &mut Tx| {
                s.toggle_home(tx);
            })
    };
    let search = || {
        action_row(p, "search", &ic::MAGNIFYING_GLASS, t!("command.session.search"), crate::keymap::Bind::SessionSearch.keycaps(), false)
            .matched(super::titlebar::NAV_SEARCH).matched_crop().motion(GLIDE)
            .on_press_tx(|s: &mut App, tx: &mut Tx| {
                s.close_settings();
                s.open_session_search(tx)
            })
    };
    let settings = || {
        action_row(p, "sidebar-settings", &ic::SETTINGS_GEAR, t!("sidebar.settings"), crate::keymap::Bind::Settings.keycaps(), s.settings_open())
            .on_press_tx(|s: &mut App, tx| s.toggle_settings(tx))
    };
    let automations_open = *s.route == Route::Automations && !s.settings_open();
    let automations = || {
        let count = crate::automations::active_count(s);
        let mut r = action_row(p, "vertical-tabs-automations", &crate::automations::CLOCK, t!("automations.title"), vec![], automations_open)
            .matched(super::titlebar::NAV_AUTOMATIONS).matched_crop().motion(GLIDE)
            .on_press_tx(|s: &mut App, tx: &mut Tx| crate::automations::toggle(s, tx));
        if count > 0 {
            let ink = if automations_open { p.text } else { p.faint };
            r = r.child(spacer()).child(text(count.to_string()).key("automations-count").font(TextStyle::new(12.0, 440.0).lh(16.0).features("\"tnum\"")).color(ink).group_color(p.text));
        }
        r
    };
    let show_auto = crate::automations::available(s);
    let mut out = Vec::with_capacity(4);
    if s.settings.navigation_prototype() {
        out.push(new());
        out.push(home_row());
        if show_auto { out.push(automations()); }
        out.push(settings());
    } else if s.project_navigation() {
        out.push(new());
        out.push(search());
        if show_auto { out.push(automations()); }
        out.push(settings());
    } else {
        out.push(home_row());
        out.push(search());
        if show_auto { out.push(automations()); }
        out.push(new());
    }
    out
}

/// The sidebar's list: the side tabs, or the inbox.
fn list(s: &App, p: &Pal) -> El {
    match s.tab_layout() {
        // A switch crossfades the whole list, and the active session's tab flies into its card
        // (`shell-active-{id}`), which landing the list's changes at once would keep from flying.
        crate::layout_state::TabLayout::Inbox => super::inbox::inbox(s, p, (INSET_START, INSET_END)),
        // Experimental › Organize vertical sidebar: side tabs alone (`titlebar.tsx:814-826`).
        crate::layout_state::TabLayout::Vertical => match s.settings.sidebar_organize.effective() {
            crate::settings::Organize::Customize => customized::customized(s, p),
            crate::settings::Organize::Recent => recent::recent(s, p),
            crate::settings::Organize::Consolidated if s.settings.navigation() == crate::settings::Navigation::List => {
                recent::recent_consolidated(s, p)
            }
            crate::settings::Organize::Consolidated if crate::tabs::sections::on(s) => sections::list(s, p, pinned_area::vertical(s, p, true)),
            crate::settings::Organize::Consolidated => pinned_area::side_tabs(s, p),
            crate::settings::Organize::List if crate::tabs::groups::on(s) => tab_groups::list(s, p),
            crate::settings::Organize::List | crate::settings::Organize::Projects if crate::tabs::sections::on(s) => sections::list(s, p, None),
            crate::settings::Organize::List | crate::settings::Organize::Projects => {
                tab_strip(s, p, "tabs", TabOrientation::Vertical, None, None)
            }
        },
        _ => tab_strip(s, p, "tabs", TabOrientation::Vertical, None, None),
    }
}

/// The hover peek's two zones (`sidebar-peek.ts`): resting at the window's start edge slides
/// the closed sidebar out over the panel (see `sidebar`), and straying past the corridor over
/// it and the toggle sends it back. They only read where the pointer is, never taking a press,
/// and stay mounted with the sidebar, open or closed, so neither misses an enter or a leave. The
/// window draws them first, under the sidebar: on top, a right-click on a row found them, not
/// the row, and opened no menu.
pub(crate) fn peek(s: &App) -> Vec<El> {
    if !s.shows_sidebar() {
        return vec![];
    }
    let chrome = chrome();
    let edge = el()
        .key("peek-edge")
        .pin(Some(0.0), Some(chrome.top() + EDGE_TOP), None, Some(0.0))
        .w(s.diag.knobs.get(&PEEK_EDGE))
        .on_hover_change(|s: &mut App, inside, tx: &mut Tx| {
            tx.live_only();
            let can_peek = !*s.sidebar_open && can_peek(s);
            s.queue_peek(PeekEvent::Edge { inside, can_peek, held: held(s) }, tx);
        });
    let corridor = el()
        .key("peek-corridor")
        .pin(Some(0.0), Some(chrome.row_h), None, Some(0.0))
        .w(surface_width(s) + CORRIDOR)
        .on_hover_change(|s: &mut App, inside, tx: &mut Tx| {
            tx.live_only();
            s.queue_peek(PeekEvent::Corridor { inside, held: held(s) }, tx);
        });
    vec![edge, corridor]
}

/// A menu or a rename holds an open peek.
fn held(s: &App) -> bool {
    s.popup.is_open() || s.kernel.slots.get(&crate::session_menu::MENUS).renaming().is_some()
}

/// Nothing holds the pointer elsewhere: no drag, resize or menu.
fn can_peek(s: &App) -> bool {
    !s.popup.is_open() && s.gestures.tab_drag.dragging().is_none() && matches!(*s.gestures.sidebar_drag, Resize::Idle) && s.kernel.slots.get(&crate::session_menu::MENUS).confirming().is_none()
}

/// `packages/ui/src/components/resize-handle-machine.ts`. Rune owns pointer capture
/// and disposal; this fixed-width sidebar has no collapse threshold.
#[derive(Clone, Copy, Default, PartialEq)]
pub enum Resize { #[default] Idle, Pressed(f32), Dragging(f32) }

enum ResizeEvent { Press(f32), Move(f32), Release, Cancel }

fn resize_step(state: &mut Resize, event: ResizeEvent) -> Option<f32> {
    match (*state, event) {
        (Resize::Idle, ResizeEvent::Press(w)) => *state = Resize::Pressed(w),
        (Resize::Pressed(from) | Resize::Dragging(from), ResizeEvent::Move(dx)) if dx != 0.0 || matches!(*state, Resize::Dragging(_)) => {
            *state = Resize::Dragging(from);
            return Some((from + dx).clamp(SIDEBAR_MIN, SIDEBAR_MAX));
        }
        (Resize::Dragging(from), ResizeEvent::Cancel) => { *state = Resize::Idle; return Some(from); }
        (_, ResizeEvent::Cancel | ResizeEvent::Release) => *state = Resize::Idle,
        _ => {}
    }
    None
}

/// The full 8pt draggable edge, invisible at rest, hovered, and pressed (#64219).
pub(super) fn resize_handle(w: f32) -> El {
    el().key("sidebar-resize")
        // Below a titlebar row, which moves the window instead.
        .absolute(w, chrome().row_h)
        .w(8.0)
        .h_full()
        .cursor(Cursor::ResizeColumn)
        .on_drag(|s: &mut App, d: &mut Drag, tx: &mut Tx| {
            tx.instant();
            let event = match d.phase {
                Phase::Down => ResizeEvent::Press(*s.sidebar_w),
                Phase::Move => {
                    // Dragging toward the chat widens it: right, or left in a right-to-left window.
                    let dx = if opencode_i18n::direction() == opencode_i18n::Direction::Rtl { -d.dx } else { d.dx };
                    ResizeEvent::Move(dx)
                }
                Phase::Up if d.cancelled() => ResizeEvent::Cancel,
                Phase::Up => ResizeEvent::Release,
            };
            if let Some(width) = resize_step(&mut s.gestures.sidebar_drag, event) { *s.sidebar_w = width; }
        })
}

/// The sidebar toggle (`SidebarToggle`, ghost-muted large): the open glyph or the closed one,
/// with the unread badge while the closed sidebar hides an unread tab. A press docks or closes
/// the sidebar; resting on it never peeks.
pub(super) fn toggle_button(s: &App, p: &Pal, open: bool) -> El {
    let current = s.current_tab();
    let unread = !open && s.tabs.iter().any(|t| Some(t.uid()) != current && matches!(s.tab_status_of(t), Some(TabStatus::Unread | TabStatus::Failed)));
    let mut b = el()
        .key("sidebar-toggle")
        .role("button")
        .label(if unread { t!("command.sidebar.toggle.unread") } else { t!("command.sidebar.toggle") })
        .help(t!("command.sidebar.toggle")).help_keys(crate::keymap::Bind::Sidebar.keycaps())
        .size(28.0, 28.0)
        .radius(6.0)
        .focus_inset(p)
        .center()
        .hover(p.hover)
        .press(p.pressed)
        .hover_motion(INSTANT, INSTANT)
        .on_press_tx(|s: &mut App, tx: &mut Tx| {
            s.toggle_sidebar(tx);
        })
        .child(icon(&ic::LAYOUT_LEFT_FULL, 16.0).color(p.icon_muted).mirror_in_rtl().absolute(6.0, 6.0).opacity(if open { 1.0 } else { 0.0 }).opacity_motion(SIDEBAR_SPRING))
        .child(icon(&ic::LAYOUT_LEFT, 16.0).color(p.icon_muted).mirror_in_rtl().absolute(6.0, 6.0).opacity(if open { 0.0 } else { 1.0 }).opacity_motion(SIDEBAR_SPRING));
    if unread {
        // `UnreadBadge`: a 7pt accent dot on the glyph's corner in a 1.5pt ring of the ground.
        let ring = 7.0 + 3.0;
        b = b.child(el().key("unread").absolute(6.0 + 16.0 - ring / 2.0 - 1.0, 6.0 - ring / 2.0 + 1.0).size(ring, ring).radius(ring / 2.0).fill(p.bg_deep).center().child(el().size(7.0, 7.0).radius(3.5).fill(p.bg_accent)));
    }
    b
}

/// One toggle that reads as the header's and the collapsed one: it rides the sidebar's edge
/// to the collapsed spot past the traffic lights while its glyph turns (`sidebar-toggle.tsx`).
pub(super) fn sidebar_toggle(s: &App, p: &Pal, open: bool, w: f32) -> El {
    let (x, y) = chrome().toggle_at(open, w);
    let toggle = toggle_button(s, p, open).absolute(x, y).motion(SIDEBAR_SPRING);
    // Above a peek, which is raised over the panel.
    if s.layout.peek.shown() { toggle.raise() } else { toggle }
}

/// Room the session header keeps for the collapsed toggle.
pub(crate) fn collapsed_slot(s: &App) -> El {
    // Beta measures from the header's 10px padding; this header pads 14.
    let open = *s.sidebar_open || !s.shows_sidebar();
    el().key("toggle-space").w(chrome().toggle_space(open)).h(28.0).motion(SIDEBAR_SPRING)
}

/// Desktop's activity spinner, "Soft Radar": a 5×5 dot matrix whose sweep head steps clockwise
/// around each square ring, leaving a fading trail (`spinner-radar.ts`), 14px, icon-muted.
pub(crate) use session_view::style::{activity_ring, activity_ring_sized};
