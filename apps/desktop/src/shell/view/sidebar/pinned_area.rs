//! Consolidated navigation's pinned area (`packages/app/src/shell/titlebar/pinned-area.tsx`,
//! `pinned-area.css`, #64668): loose pinned sessions and accordion-collapsible colored pin groups,
//! rendered vertically in side tabs / List and horizontally as icon-only pinned tabs in top tabs.

use rune_kit::shell::{TabItemStatus, TabOrientation};

use super::*;
use crate::popup::{self, Item, Menu, MenuButton as _};
use crate::session_menu::{Spot, Target, preferences};
use crate::tabs::pins::{self, HiddenMark, PinColor, PinGroup, PinItem, PinRef};
use opencode_i18n::t;

/// `sessions-stack` (`packages/ui/src/icons/catalog.ts:393`): Home's glyph in every layout
/// (`packages/app/src/shell/titlebar/titlebar.tsx:427,438`, `opencode#66594`).
pub(crate) static SESSIONS_STACK: Icon = Icon {
    name: "sessions-stack",
    view: 16.0,
    body: r#"<path d="M3.5 3.5H12.5M1.5 5.5V12.5H14.5V5.5H12.5H3.5H1.5Z" stroke-linecap="round" stroke-linejoin="round" stroke="currentColor"/>"#,
};

/// Home's label and glyph (`homeLabel`, `titlebar.tsx:405-445`, `opencode#66594`):
/// Sessions under Consolidated, Home otherwise; always uses `sessions-stack`.
pub(crate) fn home_label(s: &App) -> (&'static str, &'static Icon) {
    let label = if s.settings.navigation_prototype() { t!("sidebar.sessions") } else { t!("home.title") };
    (label, &SESSIONS_STACK)
}

/// Group marker colors matching `pinned-area.css` (`--pinned-group-bg` / `--pinned-group-border`)
/// and `tab-groups.css` (`--tab-group-pill-border`).
fn group_colors(c: PinColor, dark: bool) -> (Color, Color, Color) {
    use rune::{oklch, oklcha};
    let (h, bd, bl) = match c {
        PinColor::Blue => (254.0, 0.12, 0.11),
        PinColor::Cyan => (206.0, 0.10, 0.095),
        PinColor::Green => (158.0, 0.10, 0.095),
        PinColor::Orange => (56.0, 0.11, 0.10),
        PinColor::Purple => (292.0, 0.12, 0.11),
        PinColor::Pink => (350.0, 0.11, 0.10),
        PinColor::Gray => (260.0, 0.012, 0.01),
    };
    if dark {
        (oklch(0.62, bd, h), oklcha(0.75, bd, h, 0.55), oklcha(0.42, bd * 0.85, h, 0.55))
    } else {
        (oklch(0.64, bl, h), oklcha(0.45, bl, h, 0.45), oklcha(0.78, bl * 0.9, h, 0.72))
    }
}

/// 10×10 rounded-2 colored square (`[data-slot="pinned-group-rect"]`).
fn group_square(c: PinColor, dark: bool) -> El {
    let (bg, border, _) = group_colors(c, dark);
    el().key("group-square")
        .size(10.0, 10.0)
        .radius(2.0)
        .fill(bg)
        .box_shadow(Pal::ring_in(border))
        .a11y_hidden()
}

fn hidden_dot(p: &Pal, mark: HiddenMark, vertical: bool) -> El {
    let color = match mark {
        HiddenMark::Attention => p.warning,
        HiddenMark::Unread => p.accent,
    };
    let end = if vertical { 0.0 } else { -3.0 };
    el().key("group-mark")
        .pin(None, Some(-3.0), Some(end), None)
        .size(6.0, 6.0)
        .radius(3.0)
        .fill(color)
        .a11y_hidden()
}

/// Consolidated side tabs: pinned area above, divider, then unpinned open tabs, in their groups
/// under Settings › Group tabs (`titlebar.tsx:708-729`).
pub(crate) fn side_tabs(s: &App, p: &Pal) -> El {
    let area = vertical(s, p, true);
    let groups = crate::tabs::auto_groups::groups(s);
    if area.is_none() && groups.is_none() {
        return crate::view::tab_strip::tab_strip(s, p, "tabs", TabOrientation::Vertical, None, None);
    }
    let rest: Vec<usize> = match groups {
        Some(_) => vec![],
        None => s.tabs.iter().enumerate().filter(|(_, t)| !pins::holds(s, t)).map(|(i, _)| i).collect(),
    };
    let mut content = col().key("consolidated-content").w_full().children(area);
    if let Some(groups) = groups {
        content = content.child(super::auto_sections::sections(s, p, &groups));
    } else if !rest.is_empty() {
        let tabs_h = rune::memo::layout_with("tabs-list", |r| r[3]).unwrap_or(rest.len() as f32 * 30.0);
        let strip = col()
            .key("consolidated-tabs")
            .w_full()
            .h(tabs_h)
            .on_layout_tx(|s: &mut App, r: [f32; 4], tx: &mut Tx| {
                tx.live_only();
                s.strip_laid_out(r);
            })
            .child(crate::view::tab_strip::section_strip(s, p, "tabs", &rest, false));
        content = content.child(strip);
    }
    let viewport = col().grow(1.0).shrink().min_h(0.0);
    let scroll = session_view::scroller::scroller("consolidated-scroll").build::<App>(p, viewport, content);
    col().key("consolidated").w_full().grow(1.0).shrink().min_h(0.0).child(scroll)
}

/// Vertical `PinnedArea` for side tabs (`divider = true`) and Consolidated List (`divider = false`).
pub(crate) fn vertical(s: &App, p: &Pal, divider: bool) -> Option<El> {
    let doc = &pins::state(s).doc;
    if doc.items.is_empty() {
        return None;
    }
    let nav_order = s.nav_order();
    let show_hints = crate::tab_hints::shown(s);
    let mut area = col()
        .key("pinned-area")
        .role("group")
        .label_static(t!("sidebar.recent.pinned"))
        .w_full()
        .gap(3.0)
        .child(super::rows::section_title(p, "pinned-area-title", t!("sidebar.recent.pinned")));
    if !divider {
        area = area.pb(16.0);
    }
    for item in &doc.items {
        area = match item {
            PinItem::Session(r) => area.child(vertical_session(s, p, r, &nav_order, show_hints)),
            PinItem::Group(g) => area.child(vertical_group(s, p, g, &nav_order, show_hints)),
        };
    }
    let has_unpinned = s.tabs.iter().any(|t| !pins::holds(s, t));
    if divider && has_unpinned {
        area = area.child(el().key("pinned-divider").mx(8.0).my(12.0).h(1.0).fill(p.border_muted).a11y_hidden());
    }
    Some(area)
}

/// Horizontal `PinnedArea` for top tabs (`pinned-area.tsx`, `orientation="horizontal"`):
/// leading divider, icon-only pinned sessions and outlined groups, trailing divider.
pub(crate) fn horizontal(s: &App, p: &Pal) -> Option<El> {
    if !s.settings.navigation_prototype() {
        return None;
    }
    let doc = &pins::state(s).doc;
    if doc.items.is_empty() {
        return None;
    }
    let nav_order = s.nav_order();
    let show_hints = crate::tab_hints::shown(s);
    let sep = |key: &'static str| el().key(key).mx(4.0).w(1.0).h(16.0).fill(p.border_muted).a11y_hidden();
    let mut bar = row()
        .key("pinned-area-horizontal")
        .role("group")
        .label_static(t!("sidebar.recent.pinned"))
        .gap(4.0)
        .center_y()
        .child(sep("pinned-sep-start"));
    for item in &doc.items {
        bar = match item {
            PinItem::Session(r) => bar.child(horizontal_session(s, p, r, &nav_order, show_hints)),
            PinItem::Group(g) => bar.child(horizontal_group(s, p, g, &nav_order, show_hints)),
        };
    }
    Some(bar.child(sep("pinned-sep-end")))
}

struct SessionDisplay<'a> {
    title: String,
    project: Option<&'a model::Project>,
    status: Option<TabItemStatus>,
    muted: bool,
    active: bool,
    shortcut: Option<usize>,
}

fn resolve_session<'a>(s: &'a App, r: &PinRef, nav_order: &[String]) -> SessionDisplay<'a> {
    let tab = s.tabs.iter().find(|t| !t.is_draft() && t.is(&r.server, &r.session_id));
    let info = (r.server == *crate::servers::runtime::active(s)).then(|| s.session(&r.session_id)).flatten();
    let closed = s.gestures.closed.entries().find(|c| c.tab.is(&r.server, &r.session_id));
    let raw_title = info
        .map(|i| i.title.as_str())
        .or_else(|| tab.map(|t| t.title.as_str()))
        .or_else(|| closed.map(|c| c.tab.title.as_str()))
        .unwrap_or("");
    let title = crate::tabs::tab_title(raw_title);
    let project = info
        .and_then(|i| s.project_for_session(i))
        .or_else(|| tab.and_then(|t| crate::view::tab_strip::face(s, t).project))
        .or_else(|| s.checkout.project(&r.server, &r.session_id));
    let status = pins::status_of(s, r).map(|st| match st {
        crate::TabStatus::Busy => TabItemStatus::Busy,
        crate::TabStatus::Attention => TabItemStatus::Attention,
        crate::TabStatus::Unread => TabItemStatus::Unread,
        crate::TabStatus::Failed => TabItemStatus::Failed,
    });
    let muted = preferences::muted(&r.server, &r.session_id);
    let active = pins::is_current(s, r);
    let uid = r.uid();
    let shortcut = nav_order.iter().position(|u| u == &uid).filter(|&i| i < 9).map(|i| i + 1);
    SessionDisplay { title, project, status, muted, active, shortcut }
}

fn vertical_session(s: &App, p: &Pal, r: &PinRef, nav_order: &[String], show_hints: bool) -> El {
    let d = resolve_session(s, r, nav_order);
    let target = Target::on(&r.server, &r.session_id, Spot::Row);
    let marked = s.kernel.slots.get(&crate::session_menu::MENUS).on.as_ref() == Some(&target);
    let avatar = crate::icons::tab_badge(s, p, d.project, &d.title, d.shortcut, show_hints);
    let mark = match d.status {
        Some(TabItemStatus::Busy) => Some(session_view::style::tab_busy_glyph(crate::palette::kit(p), 14.0)),
        _ if d.muted => Some(super::pins::muted_mark(p)),
        Some(TabItemStatus::Attention) => Some(el().key("attention").size(7.0, 7.0).radius(3.5).fill(p.warning).a11y_hidden()),
        Some(TabItemStatus::Unread | TabItemStatus::Failed) if !d.active => Some(el().key("unread").size(7.0, 7.0).radius(3.5).fill(p.accent).a11y_hidden()),
        _ => None,
    };
    let ink = if d.active || d.status.is_some() { p.text } else { p.muted };
    let title_el = match crate::session_menu_view::field(s, p, &target, UI_MED) {
        Some(field) => field,
        None => text(d.title.clone()).font(UI).color(ink).grow(1.0).shrink().min_w(0.0).truncate_fade().dir(rune::Direction::Auto),
    };
    let pin_click = r.clone();
    let pin_ctx = r.clone();
    let pin_close = r.clone();
    let mut item = row()
        .key(format!("pin-{}", r.key()))
        .role("tab")
        .label(d.title.clone())
        .focus_selected(d.active)
        .w_full()
        .h(28.0)
        .pl(8.0)
        .pr(4.0)
        .gap(8.0)
        .center_y()
        .radius(6.0)
        .focus_inset(p)
        .fill(if d.active { p.pressed } else if marked { p.hover } else { Color::TRANSPARENT })
        .hover(if d.active || marked { Color::TRANSPARENT } else { p.hover })
        .hover_motion(INSTANT, INSTANT)
        .on_press_tx(move |s: &mut App, tx: &mut Tx| pins::open_pin(s, &pin_click, tx))
        // A middle click unpins it and closes its tab (`onMiddleClick`, opencode#66265).
        .closable_tab(move |s: &mut App, tx: &mut Tx| pins::close(s, &pin_close, tx))
        .on_secondary_press(move |s: &mut App, press: rune::PointerPress, tx: &mut Tx| open_pin_menu(s, &pin_ctx, press, tx))
        .child(el().size(16.0, 16.0).center().child(avatar))
        .child(title_el);
    if d.muted {
        item = item.description(super::pins::muted_description(d.status));
    }
    if let Some(m) = mark {
        // Matches `TabNavItem` (`pinned-area.tsx:545-565`, `tab-nav.css:1-8`): 20×20 slot 4 pt from end.
        item = item.child(el().key("status").size(20.0, 20.0).center().child(m));
    }
    item
}

fn horizontal_session(s: &App, p: &Pal, r: &PinRef, nav_order: &[String], show_hints: bool) -> El {
    let d = resolve_session(s, r, nav_order);
    let target = Target::on(&r.server, &r.session_id, Spot::Row);
    let marked = s.kernel.slots.get(&crate::session_menu::MENUS).on.as_ref() == Some(&target);
    let avatar = if d.status == Some(TabItemStatus::Busy) && !show_hints {
        session_view::style::tab_busy_glyph(crate::palette::kit(p), 14.0)
    } else {
        crate::icons::tab_badge(s, p, d.project, &d.title, d.shortcut, show_hints)
    };
    let dot = match d.status {
        Some(TabItemStatus::Attention) => Some(p.warning),
        Some(TabItemStatus::Unread | TabItemStatus::Failed) if !d.active && !d.muted => Some(p.accent),
        _ => None,
    };
    let mut lead = el().size(16.0, 16.0).center().child(avatar);
    if let Some(color) = dot {
        lead = lead.child(el().key("dot").pin(None, Some(-2.0), Some(-2.0), None).size(6.0, 6.0).radius(3.0).fill(color).a11y_hidden());
    }
    let pin_click = r.clone();
    let pin_ctx = r.clone();
    let pin_close = r.clone();
    let mut slot = row()
        .key(format!("pin-h-{}", r.key()))
        .role("tab")
        .label(d.title.clone())
        .help(d.title)
        .focus_selected(d.active)
        .size(28.0, 28.0)
        .center()
        .radius(6.0)
        .focus_inset(p)
        .fill(if d.active { p.pressed } else if marked { p.hover } else { Color::TRANSPARENT })
        .hover(if d.active || marked { Color::TRANSPARENT } else { p.hover })
        .hover_motion(INSTANT, INSTANT)
        .on_press_tx(move |s: &mut App, tx: &mut Tx| pins::open_pin(s, &pin_click, tx))
        // A middle click unpins it and closes its tab (`onMiddleClick`, opencode#66265).
        .closable_tab(move |s: &mut App, tx: &mut Tx| pins::close(s, &pin_close, tx))
        .on_secondary_press(move |s: &mut App, press: rune::PointerPress, tx: &mut Tx| open_pin_menu(s, &pin_ctx, press, tx))
        .child(lead);
    if d.muted {
        slot = slot.description(super::pins::muted_description(d.status));
    }
    slot
}

/// A fold header's chevron and its turn (`transition-[transform,opacity]`: 150 ms, Tailwind's
/// default ease).
const CHEVRON: Curve = Curve::tween(0.15, Easing::cubic(0.4, 0.0, 0.2, 1.0));

/// A header's chevron that shows with the pointer over the header's row or with the keyboard's
/// focus on its toggle (the ancestor keyed `toggle`), fading over 150 ms, and always where the
/// pointer cannot hover (`pinned-area.tsx:376`, `recent-sessions.tsx:1222-1225`: `opacity-0
/// transition-[transform,opacity] group-hover/row:opacity-100
/// group-focus-visible/toggle:opacity-100 [@media(hover:none)]:opacity-100`, opencode#66265).
/// It turns over the same 150 ms.
pub(super) fn group_chevron(p: &Pal, open: bool, toggle: &str) -> El {
    let chevron = icon(&ic::FILL_TRIANGLE_DOWN, 14.0)
        .color(p.icon_muted)
        .rotate(if open { 0.0 } else { -std::f32::consts::FRAC_PI_2 })
        .mirror_in_rtl()
        .rotate_motion(CHEVRON);
    if session_view::style::hover_none() {
        return chevron;
    }
    chevron.group_opacity(0.0, 1.0).group_focus_within(toggle.to_owned()).group_motion(CHEVRON, CHEVRON)
}

fn vertical_group(s: &App, p: &Pal, g: &PinGroup, nav_order: &[String], show_hints: bool) -> El {
    let open = !pins::state(s).doc.collapsed.get(&g.id).copied().unwrap_or(false);
    let mark = pins::hidden_mark(s, g);
    let marker = el()
        .key("marker")
        .mx(3.0)
        .size(10.0, 10.0)
        .center()
        .child(group_square(g.color, p.dark))
        .children(mark.map(|m| hidden_dot(p, m, true)));

    let label_el = text(g.name.clone()).font(UI_MED).color(p.text).shrink().min_w(0.0).truncate().dir(rune::Direction::Auto);

    let toggle_id = g.id.clone();
    let ctx_id = g.id.clone();
    let menu_key = format!("pin-group-menu-{}", g.id);
    let menu_open = s.popup.open_at(&menu_key);
    let trigger_id = g.id.clone();
    let trigger_key = menu_key.clone();
    let ctx_key = menu_key.clone();

    let toggle_key = format!("pin-group-trigger-{}", g.id);
    let trigger = row()
        .key(toggle_key.clone())
        .role("button")
        .label(g.name.clone())
        .expanded(open)
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h(28.0)
        .pl(8.0)
        .pr(32.0)
        .gap(8.0)
        .center_y()
        .radius(6.0)
        .focus_inset(p)
        // Commit without `tx.instant()` so the chevron rotates smoothly over 150 ms (`CHEVRON`,
        // `packages/app/src/shell/titlebar/pinned-area.tsx:376`, `opencode#66265`).
        .on_press_tx(move |s: &mut App, _tx: &mut Tx| {
            pins::update(s, |d| d.toggle_group(&toggle_id));
        })
        // A right-click opens the menu where the row's `⋯` does, as official's `Menu` (`bottom-end`).
        .on_secondary_press(move |s: &mut App, _: rune::PointerPress, tx: &mut Tx| open_group_menu(s, &ctx_id, ctx_key.clone().into(), rune::Align::End, tx))
        // The toggle's fill follows the ROW's hover, the pointer over the row's menu button
        // too (`pinned-area.tsx:350`: `group-hover/row:bg-v2-overlay-simple-overlay-hover`), and
        // not the keyboard's focus, which draws the ring alone. It is a layer of the row's
        // group: a hover colour on the toggle itself would make the toggle the group of what
        // it holds.
        .child(el().key("hover").pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0)).radius(6.0).fill(p.hover).group_opacity(0.0, 1.0).a11y_hidden())
        .child(marker)
        .child(label_el)
        .child(group_chevron(p, open, &toggle_key));

    // The menu button shows with the pointer over the row or with focus on itself
    // (`pinned-area.tsx:417`: `opacity-0 group-hover/row:opacity-100 focus-visible:opacity-100`).
    // Only the pointer lights the row, so focus on the toggle doesn't show it.

    let dots = super::super::home::projects::icon_button(p, &menu_key, t!("navigation.group.options"), &ic::OUTLINE_DOTS, menu_open)
        .menu_button(menu_key.clone(), move |s: &mut App, tx: &mut Tx| open_group_menu(s, &trigger_id, trigger_key.clone().into(), rune::Align::End, tx));
    let dots = el().pin(None, Some(4.0), Some(4.0), None).child(if menu_open { dots } else { session_view::style::hover_reveal(dots) });
    let header_row = row().key(format!("pin-group-row-{}", g.id)).w_full().hover_group_pointer().child(trigger).child(dots);

    let mut group_col = col().key(format!("pin-group-{}", g.id)).w_full().gap(3.0).child(header_row);
    let visible = pins::visible_in_group(s, g);
    if !visible.is_empty() {
        let mut sessions = col().key("sessions").w_full().pl(24.0).gap(3.0);
        // Logical start rail (`inset-inline-start: 15.5px`), RTL-safe.
        sessions = sessions.child(el().key("rail").pin(Some(15.5), Some(2.0), None, Some(2.0)).w(1.0).radius(1.0).fill(p.border_muted).a11y_hidden());
        for r in visible {
            sessions = sessions.child(vertical_session(s, p, r, nav_order, show_hints));
        }
        group_col = group_col.child(sessions);
    }
    group_col
}

fn horizontal_group(s: &App, p: &Pal, g: &PinGroup, nav_order: &[String], show_hints: bool) -> El {
    let open = !pins::state(s).doc.collapsed.get(&g.id).copied().unwrap_or(false);
    let mark = pins::hidden_mark(s, g);
    let (_, _, outline) = group_colors(g.color, p.dark);
    let marker = el()
        .key("marker")
        .size(10.0, 10.0)
        .center()
        .child(group_square(g.color, p.dark))
        .children(mark.map(|m| hidden_dot(p, m, false)));
    let toggle_id = g.id.clone();
    let ctx_id = g.id.clone();
    let ctx_key = format!("pin-group-h-{}", g.id);
    let menu_open = s.popup.open_at(&ctx_key);
    let trigger = row()
        .key(format!("pin-group-h-trigger-{}", g.id))
        .role("button")
        .label(g.name.clone())
        .expanded(open)
        .size(28.0, 28.0)
        .center()
        .radius(6.0)
        .focus_inset(p)
        .hover(if menu_open { Color::TRANSPARENT } else { p.hover })
        // Commit without `tx.instant()` so the chevron rotates smoothly over 150 ms (`CHEVRON`,
        // `packages/app/src/shell/titlebar/pinned-area.tsx:376`, `opencode#66265`).
        .on_press_tx(move |s: &mut App, _tx: &mut Tx| {
            pins::update(s, |d| d.toggle_group(&toggle_id));
        })
        // Top tabs open it under the group's start (official's hidden `Menu.Trigger`, `bottom-start`).
        .on_secondary_press(move |s: &mut App, _: rune::PointerPress, tx: &mut Tx| open_group_menu(s, &ctx_id, ctx_key.clone().into(), rune::Align::Start, tx))
        .child(marker);
    let trigger = if menu_open { trigger.fill(p.pressed) } else { trigger.help(g.name.clone()) };
    // No gap between the group's square and its sessions (`pinned-area.tsx:390`, opencode#66343).
    let mut r = row()
        .key(format!("pin-group-h-{}", g.id))
        .center_y()
        .radius(8.0)
        .box_shadow(Pal::ring_in(outline))
        .child(trigger);
    for ses in pins::visible_in_group(s, g) {
        r = r.child(horizontal_session(s, p, ses, nav_order, show_hints));
    }
    r
}

fn open_pin_menu(s: &mut App, r: &PinRef, press: rune::PointerPress, tx: &mut Tx) {
    // `pinned-area.tsx:546-559` (opencode#66576): Rename, PinMenuItems, Mark read/unread, ---, Copy >, Export….
    let target = Target::on(&r.server, &r.session_id, Spot::Row);
    let (server, id) = (r.server.clone(), r.session_id.clone());
    let read = crate::servers::runtime::with_server(s, &server, |s| {
        crate::session_menu::lines(s, &target).into_iter().find(|line| {
            matches!(line, crate::session_menu::Line::Item(i) if matches!(i.id, "read" | "unread"))
        })
    })
    .flatten();
    let copy_sub = crate::servers::runtime::with_server(s, &server, |s| {
        crate::session_menu::copy::submenu(s, &server, &id)
    })
    .flatten();
    let known = copy_sub.is_some();
    let mut lines = vec![crate::session_menu::Line::Extra(Item::new("rename", t!("common.rename")))];
    lines.extend(pins::menu_lines(s, &server, &id));
    if let Some(read_line) = read {
        lines.push(read_line);
    }
    lines.push(crate::session_menu::Line::Sep);
    if let Some(sub) = copy_sub {
        lines.push(sub);
    }
    lines.push(crate::session_menu::Line::Extra(
        Item::new("export", format!("{}\u{2026}", t!("common.export"))).enabled(known && !s.exporting(&server, &id)),
    ));
    let menu = crate::session_menu::menu(&lines);
    s.kernel.slots.get_mut(&crate::session_menu::MENUS).on = Some(target.clone());
    tx.instant();
    popup::open(s, menu, press, tx, move |s: &mut App, picked, tx| {
        s.kernel.slots.get_mut(&crate::session_menu::MENUS).on = None;
        tx.instant();
        match picked {
            Some("rename") => crate::session_menu::rename_dialog::open(s, &target),
            Some(action @ ("export" | "read" | "unread")) => crate::session_menu::run(s, &target, action, tx),
            Some(action) if action.starts_with("copy:") => crate::session_menu::run(s, &target, action, tx),
            Some(menu_id) => {
                pins::run_menu(s, &server, &id, menu_id);
            }
            None => {}
        }
    });
}

fn open_group_menu(s: &mut App, group_id: &str, at: rune::PopupAnchor, align: rune::Align, tx: &mut Tx) {
    let Some(g) = pins::state(s).doc.groups().find(|g| g.id == group_id).cloned() else { return };
    // `GroupEditor` (`pinned-area.tsx:449-489`): the group's name in a bare field, its square
    // before it, then its colours as a radio list, then Ungroup and Unpin, 192 wide (`w-48`).
    let (lead_id, edit_id, rows_id, color_id, id) = (g.id.clone(), g.id.clone(), g.id.clone(), g.id.clone(), g.id.clone());
    let lead = move |s: &App, p: &Pal| {
        let color = pins::state(s).doc.groups().find(|g| g.id == lead_id).map(|g| g.color);
        color.map_or_else(el, |c| group_square(c, p.dark))
    };
    let rename = move |s: &mut App, name: &str, _: &mut Tx| pins::update(s, |d| d.rename_group(&edit_id, name.to_owned()));
    // A colour is a radio row: it recolours the group and the menu stays open.
    let recolor = move |s: &mut App, choice: &str, _: &mut Tx| {
        if let Some(c) = choice.strip_prefix("color:").and_then(|c| PinColor::parse(Some(c))) {
            pins::update(s, |d| d.recolor_group(&color_id, c));
        }
    };
    let rows = move |s: &App| {
        let current = pins::state(s).doc.groups().find(|g| g.id == rows_id).map(|g| g.color);
        let menu = Menu::new(t!("tabs.groups.color")).width(192.0).align(align).separator();
        pins::AUTO_COLORS
            .into_iter()
            .fold(menu, |m, c| {
                let ((lf, le, _), (df, de, _)) = (group_colors(c, false), group_colors(c, true));
                m.item(Item::new(format!("color:{}", c.name()), c.label()).swatch([lf, le], [df, de]).radio(Some(c) == current))
            })
            .separator()
            .item(Item::new("ungroup", t!("tabs.groups.ungroup")))
            .item(Item::new("unpin-group", t!("navigation.group.unpin")))
    };
    tx.instant();
    popup::open_with_field(s, t!("tabs.groups.name.placeholder"), &g.name, lead, rename, recolor, rows, at, tx, move |s: &mut App, picked, tx| {
        tx.instant();
        match picked {
            Some("ungroup") => pins::update(s, |d| d.ungroup(&id)),
            Some("unpin-group") => pins::update(s, |d| d.unpin_group(&id)),
            _ => {}
        }
    });
}

rune_register::register!(crate::scenes::SCENE_LIST: crate::scenes::Scene = [
    // Consolidated Tabs: one loose pinned session + one colored pin group (2 sessions), side tabs.
    ("sidebar-consolidated", |s| {
        crate::scenes::open_scene(s);
        s.finish_stream();
        crate::scenes::side_tabs(s);
        s.settings.sidebar_organize = crate::settings::Organize::Consolidated;
        s.settings.navigation = crate::settings::Navigation::Tabs;
        *s.layout = crate::layout_state::Shell::at_rest(s.settings.effective_tab_layout());
        let server = crate::servers::runtime::active(s).clone();
        let ids: Vec<String> = s.sessions.iter().filter(|i| i.parent.is_none()).take(3).map(|i| i.id.clone()).collect();
        if let Some(first) = ids.first() {
            pins::update(s, |d| d.pin(PinRef::new(server.clone(), first)));
        }
        if let Some(second) = ids.get(1) {
            pins::update(s, |d| d.create_group(PinRef::new(server.clone(), second), "g1".into(), "Focus".into(), PinColor::Blue));
        }
        if let Some(third) = ids.get(2) {
            pins::update(s, |d| d.move_to_group(PinRef::new(server.clone(), third), "g1"));
        }
    }),
    // Consolidated Tabs in Top tabs with horizontal pinned area & group outline.
    ("top-consolidated", |s| {
        crate::scenes::open_scene(s);
        s.finish_stream();
        s.settings.sidebar_organize = crate::settings::Organize::Consolidated;
        s.settings.navigation = crate::settings::Navigation::Tabs;
        s.settings.tab_layout = crate::layout_state::TabLayout::Horizontal;
        *s.layout = crate::layout_state::Shell::at_rest(s.settings.effective_tab_layout());
        let server = crate::servers::runtime::active(s).clone();
        let ids: Vec<String> = s.sessions.iter().filter(|i| i.parent.is_none()).take(3).map(|i| i.id.clone()).collect();
        if let Some(first) = ids.first() {
            pins::update(s, |d| d.pin(PinRef::new(server.clone(), first)));
        }
        if let Some(second) = ids.get(1) {
            pins::update(s, |d| d.create_group(PinRef::new(server.clone(), second), "g1".into(), "Focus".into(), PinColor::Blue));
        }
        if let Some(third) = ids.get(2) {
            pins::update(s, |d| d.move_to_group(PinRef::new(server.clone(), third), "g1"));
        }
    }),
]);
