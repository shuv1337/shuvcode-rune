//! The Recent sidebar (Settings › Experimental › Organize vertical sidebar, Recent & Consolidated
//! List; local and dev builds only; `recent-sessions.tsx`, #64668):
//! - Under `Organize::Recent`: pinned sessions from `sidebar.recent`, then root sessions active in
//!   the last 7 days by recency (at most 30 until Show more reveals 30 more).
//! - Under `Organize::Consolidated` + `Navigation::List`: `PinnedArea` (`navigation.pins`) above,
//!   then Recent organized by `recent_view` (Group by, Sort by, Direction, Show Metadata/Empty
//!   groups, collapsible group headers with attention/unread dots, and View all -> Sessions).

use std::collections::HashSet;

use rune_kit::shell::{TabItem, TabItemStatus, TabOrientation, TabStrip};

use super::recent_view::{self, GroupBy, RecentStatus, ViewGroup, ViewRow};
use super::rows::section_title;
use super::state::key;
use super::*;
use crate::popup::{self, Item, Menu, MenuButton as _};
use crate::session_menu::preferences::{self, Kind};
use crate::session_menu::{Spot, Target};
use opencode_i18n::t;

/// `RECENT_WINDOW_MS` and `RECENT_VISIBLE_LIMIT`.
const WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;
pub(crate) const VISIBLE: usize = 30;

/// The pinned and recent sessions (`partitionRecentSessions`, `windowRecentSessions`), and
/// whether Show more has more to show.
fn partition(s: &App) -> (Vec<&SessionInfo>, Vec<&SessionInfo>, bool) {
    let here = crate::servers::runtime::active(s);
    let st = &s.project_state.recent;
    let shown = |i: &&SessionInfo| !preferences::hidden(here, &i.id);
    let pinned: Vec<&SessionInfo> = st
        .pins
        .iter()
        .filter_map(|k| super::state::split(k))
        .filter(|(server, _)| *server == here.as_str())
        .filter_map(|(_, id)| s.session(id))
        .filter(|i| i.archived.is_none())
        .filter(shown)
        .collect();
    let mut recent: Vec<&SessionInfo> = s
        .sessions
        .iter()
        .filter(|i| i.parent.is_none() && i.archived.is_none() && !crate::view::is_automation(&i.title) && !pinned.iter().any(|p| p.id == i.id))
        .filter(shown)
        .collect();
    recent.sort_by_key(|sesh| (std::cmp::Reverse(s.recent_page.time(sesh)), &sesh.id));
    let (visible, more) = window_recent(s, recent);
    (pinned, visible, more)
}

fn window_recent<'a>(s: &App, mut recent: Vec<&'a SessionInfo>) -> (Vec<&'a SessionInfo>, bool) {
    let cutoff = *s.now_ms - WINDOW_MS;
    let in_window = recent.iter().position(|i| s.recent_page.time(i) < cutoff).unwrap_or(recent.len());
    let limit = in_window.min(VISIBLE) + s.project_state.recent.revealed;
    let more = recent.len() > limit;
    recent.truncate(limit);
    (recent, more)
}

/// The Recent list in `Organize::Recent`.
pub(crate) fn recent(s: &App, p: &Pal) -> El {
    let (pinned, recent, more) = partition(s);
    let mut list = col().key("recent-content").w_full().pb(4.0);
    if !pinned.is_empty() {
        let ids: Vec<String> = pinned.iter().map(|i| i.id.clone()).collect();
        list = list.child(
            col().key("recent-pinned").role("group").label(t!("sidebar.recent.pinned")).w_full().gap(8.0).pb(10.0)
                .child(section_title(p, "recent-pinned-title", t!("sidebar.recent.pinned")))
                .child(strip(s, p, "recent-pinned-tabs", &pinned, Some(ids), false, false)),
        );
    }
    if !recent.is_empty() || more {
        let mut group = col().key("recent-list").role("group").label(t!("sidebar.recent.section.recent")).w_full().gap(8.0).pb(10.0)
            .child(section_title(p, "recent-list-title", t!("sidebar.recent.section.recent")))
            .child(strip(s, p, "recent-tabs", &recent, None, false, false));
        if more {
            group = group.child(super::rows::ghost_button(p, "recent-more", t!("sidebar.inbox.showMore"), false).on_press_tx(|s: &mut App, tx: &mut Tx| {
                tx.instant();
                s.project_state.recent.revealed += VISIBLE;
            }));
        }
        list = list.child(group);
    }
    if pinned.is_empty() && recent.is_empty() && !more {
        list = list.child(text(t!("sidebar.recent.empty")).key("recent-empty").font(UI).color(p.faint).px(6.0).py(8.0));
    }
    let viewport = col().grow(1.0).shrink().min_h(0.0);
    let scroll = session_view::scroller::scroller("recent-scroll").build::<App>(p, viewport, list);
    col().key("sidebar-recent").w_full().grow(1.0).shrink().min_h(0.0).child(scroll)
}

/// The Recent group header button currently under the pointer (not its Group and sort button),
/// so only hovering the heading button brightens the label (`recent-sessions.tsx:1201`, `opencode#66265`).
pub(crate) static HOVERED_HEADER: opencode_kernel::Slot<Option<String>> = opencode_kernel::Slot::new();

/// Visible session UIDs for Consolidated List (`nav_order()`).
pub(crate) fn consolidated_visible_uids(s: &App) -> Vec<String> {
    let here = crate::servers::runtime::active(s);
    let view = recent_view::current(s);
    let (groups, _) = consolidated_groups(s);
    let mut out = Vec::new();
    for g in &groups {
        for ses in shown_rows(s, view, g) {
            out.push(crate::tabs::session_uid(here, &ses.id));
        }
    }
    out
}

fn recent_status(s: &App, here: &opencode_kit::servers::ServerKey, id: &str) -> RecentStatus {
    let muted = preferences::muted(here, id);
    match s.tab_status(id) {
        Some(crate::TabStatus::Attention) => RecentStatus::Attention,
        Some(crate::TabStatus::Busy) => RecentStatus::Working,
        Some(crate::TabStatus::Unread | crate::TabStatus::Failed) if !muted => RecentStatus::Unread,
        _ => RecentStatus::Read,
    }
}

fn consolidated_groups(s: &App) -> (Vec<ViewGroup<&SessionInfo>>, bool) {
    let here = crate::servers::runtime::active(s);
    let view = recent_view::current(s);
    let mut all: Vec<&SessionInfo> = s
        .sessions
        .iter()
        .filter(|i| {
            i.parent.is_none()
                && i.archived.is_none()
                && !crate::view::is_automation(&i.title)
                && !preferences::hidden(here, &i.id)
                && !crate::tabs::pins::holds_session(s, here, &i.id)
        })
        .collect();
    all.sort_by_key(|sesh| (std::cmp::Reverse(s.recent_page.time(sesh)), &sesh.id));
    let (mut visible, more) = window_recent(s, all.clone());
    // Status grouping includes older sessions that need attention or are running (the old Inbox).
    if view.group_by == GroupBy::Status {
        let shown: HashSet<&str> = visible.iter().map(|i| i.id.as_str()).collect();
        for ses in all {
            if shown.contains(ses.id.as_str()) {
                continue;
            }
            if matches!(recent_status(s, here, &ses.id), RecentStatus::Attention | RecentStatus::Working) {
                visible.push(ses);
            }
        }
    }
    let servers: Vec<(String, String)> = crate::servers::registry::list(s)
        .into_iter()
        .map(|e| (e.key().as_str().to_owned(), crate::servers::registry::label(&e.conn)))
        .collect();
    let rows: Vec<ViewRow<&SessionInfo>> = visible
        .into_iter()
        .map(|ses| {
            let project = s.project_for_session(ses).map(|p| (format!("{}\0{}", here.as_str(), p.directory), p.name.clone()));
            ViewRow {
                key: key(here.as_str(), &ses.id),
                title: crate::tabs::tab_title(&ses.title),
                updated: s.recent_page.time(ses),
                created: ses.created,
                status: recent_status(s, here, &ses.id),
                project,
                server: here.as_str().to_owned(),
                item: ses,
            }
        })
        .collect();
    (recent_view::build_groups(rows, view, *s.now_ms, &servers), more)
}

fn is_selected_session(s: &App, id: &str) -> bool {
    if s.settings_open() {
        return false;
    }
    let here = crate::servers::runtime::active(s);
    matches!(&*s.route, crate::Route::Session { id: cur, server } if server == here && s.root_of(cur) == id)
}

/// Collapsed group still keeps the currently viewed session visible (`shownRows`).
fn shown_rows<'a>(s: &App, view: &recent_view::RecentView, group: &ViewGroup<&'a SessionInfo>) -> Vec<&'a SessionInfo> {
    if !view.is_collapsed(&group.key) {
        return group.rows.iter().map(|r| r.item).collect();
    }
    group.rows.iter().filter(|r| is_selected_session(s, &r.item.id)).map(|r| r.item).collect()
}

fn hidden_group_mark(s: &App, view: &recent_view::RecentView, group: &ViewGroup<&SessionInfo>) -> Option<RecentStatus> {
    if !view.is_collapsed(&group.key) {
        return None;
    }
    let mut unread = false;
    for r in &group.rows {
        if is_selected_session(s, &r.item.id) {
            continue;
        }
        match r.status {
            RecentStatus::Attention => return Some(RecentStatus::Attention),
            RecentStatus::Unread => unread = true,
            _ => {}
        }
    }
    unread.then_some(RecentStatus::Unread)
}

/// Consolidated navigation's List view (`PinnedArea` + grouped/sorted Recent with `RecentViewMenu`).
pub(crate) fn recent_consolidated(s: &App, p: &Pal) -> El {
    let view = recent_view::current(s);
    let (groups, more) = consolidated_groups(s);
    let pinned_el = super::pinned_area::vertical(s, p, false);
    let has_groups = groups.iter().any(|g| !g.rows.is_empty() || view.empty_groups) || more;
    let hide_avatars = view.group_by == GroupBy::Project;

    let mut list = col().key("recent-content").w_full().pb(4.0);
    if let Some(pins) = pinned_el {
        list = list.child(pins);
    }
    if has_groups {
        let mut stack = col().key("sidebar-recent-groups").w_full().gap(10.0).pb(10.0);
        for g in &groups {
            let rows = shown_rows(s, view, g);
            let collapsible = view.group_by != GroupBy::None;
            let collapsed = view.is_collapsed(&g.key);
            let count = (view.metadata && collapsible).then_some(g.rows.len());
            let mark = hidden_group_mark(s, view, g);
            let mut section = col()
                .key(format!("recent-group-{}", g.key))
                .role("group")
                .label(g.label.clone())
                .w_full()
                .gap(8.0)
                .child(group_header(s, p, &g.key, &g.label, count, collapsible, collapsed, mark));
            if !rows.is_empty() {
                section = section.child(strip(s, p, &format!("recent-g-{}", g.key), &rows, None, true, hide_avatars));
            }
            stack = stack.child(section);
        }
        if more {
            stack = stack.child(super::rows::ghost_button(p, "recent-more", t!("sidebar.inbox.showMore"), false).on_press_tx(|s: &mut App, tx: &mut Tx| {
                tx.instant();
                s.project_state.recent.revealed += VISIBLE;
            }));
        }
        list = list.child(stack);
    } else if crate::tabs::pins::state(s).doc.items.is_empty() {
        list = list.child(text(t!("sidebar.recent.empty")).key("recent-empty").font(UI).color(p.faint).px(6.0).py(8.0));
    }
    let viewport = col().grow(1.0).shrink().min_h(0.0);
    let scroll = session_view::scroller::scroller("recent-scroll").build::<App>(p, viewport, list);
    col().key("sidebar-recent").w_full().grow(1.0).shrink().min_h(0.0).child(scroll)
}

fn group_header(
    s: &App,
    p: &Pal,
    group_key: &str,
    label: &str,
    count: Option<usize>,
    collapsible: bool,
    collapsed: bool,
    mark: Option<RecentStatus>,
) -> El {
    let trigger_key = format!("recent-view-trigger-{group_key}");
    let menu_open = s.popup.open_at(&trigger_key);
    let open_key = trigger_key.clone();
    let sliders = super::super::home::projects::icon_button(p, &trigger_key, t!("sidebar.recent.view.options"), &ic::OUTLINE_SLIDERS, menu_open)
        .focus_inset(p)
        .menu_button(trigger_key.clone(), move |s: &mut App, tx: &mut Tx| recent_view::open_menu(s, open_key.clone(), tx));
    let sliders = if menu_open { sliders } else { sliders.help(t!("sidebar.recent.view.options")) };
    let sliders = el().pin(None, Some(2.0), Some(4.0), None).child(if menu_open { sliders } else { session_view::style::hover_reveal(sliders) });

    let heading = if collapsible {
        let toggle_key = group_key.to_owned();
        let button_key = format!("recent-header-{group_key}");
        let mut btn = row()
            .key(button_key.clone())
            .role("button")
            .label(label.to_owned())
            .expanded(!collapsed)
            .grow(1.0)
            .shrink()
            .min_w(0.0)
            .h(24.0)
            .pl(8.0)
            .pr(32.0)
            .gap(4.0)
            .center_y()
            .radius(6.0)
            .focus_inset(p)
            // Commit without `tx.instant()` so the chevron rotates smoothly over 150 ms (`CHEVRON`,
            // `recent-sessions.tsx:1222-1225`, `opencode#66265`).
            .on_press_tx(move |s: &mut App, _tx: &mut Tx| {
                recent_view::toggle_collapsed(s, &toggle_key);
            })
            // Track pointer hover on the heading button itself so hovering the sibling Group and
            // sort button keeps the heading label muted while keeping the chevron visible
            // (`packages/app/src/shell/titlebar/recent-sessions.tsx:1201`, `opencode#66265`).
            .on_hover_change({
                let key = button_key.clone();
                move |s: &mut App, inside: bool, tx: &mut Tx| {
                    tx.instant();
                    let slot = s.kernel.slots.get_mut(&HOVERED_HEADER);
                    if inside {
                        *slot = Some(key.clone());
                    } else if slot.as_deref() == Some(key.as_str()) {
                        *slot = None;
                    }
                }
            })
            .child(
                text(label.to_owned())
                    .font(UI_MED)
                    .color(if s.kernel.slots.get(&HOVERED_HEADER).as_deref() == Some(button_key.as_str()) { p.text } else { p.muted })
                    .shrink()
                    .truncate(),
            );
        if let Some(n) = count {
            btn = btn.child(text(n.to_string()).font(UI_SMALL).color(p.faint));
        }
        if let Some(m) = mark {
            let dot_color = if m == RecentStatus::Attention { p.warning } else { p.accent };
            btn = btn.child(el().key("group-dot").size(6.0, 6.0).radius(3.0).fill(dot_color).a11y_hidden());
        }
        // Under Consolidated the chevron shows with the pointer over the header or the keyboard's
        // focus on the header's button, on a 150 ms fade (`recent-sessions.tsx:1222-1225`,
        // `GroupHeader`'s `hoverReveal`, opencode#66265); otherwise it always shows.
        btn.child(if s.settings.navigation_prototype() {
            super::pinned_area::group_chevron(p, !collapsed, &button_key)
        } else {
            icon(&ic::FILL_TRIANGLE_DOWN, 14.0).color(p.icon_muted).rotate(if collapsed { -std::f32::consts::FRAC_PI_2 } else { 0.0 }).mirror_in_rtl()
        })
    } else {
        row()
            .key(format!("recent-header-{group_key}"))
            .grow(1.0)
            .shrink()
            .min_w(0.0)
            .h(24.0)
            .pl(8.0)
            .pr(32.0)
            .center_y()
            .child(text(label.to_owned()).font(UI_MED).color(p.muted).shrink().truncate())
    };
    // Only the pointer lights the header (`group-hover/header`): the Group and sort button shows
    // for the header's hover or its own focus (`recent-sessions.tsx:1231`), not for focus on
    // the header's button.
    row().key(format!("recent-head-row-{group_key}")).w_full().h(24.0).center_y().hover_group_pointer().child(heading).child(sliders)
}

/// One section's rows, drawn as side tabs (`TabNavItem`, vertical): `pins` makes them the pinned
/// list (in `Organize::Recent`), which a drag reorders.
fn strip(
    s: &App,
    p: &Pal,
    strip_key: &str,
    sessions: &[&SessionInfo],
    pins: Option<Vec<String>>,
    consolidated: bool,
    hide_avatars: bool,
) -> El {
    let here = crate::servers::runtime::active(s).clone();
    let selected = match &*s.route {
        crate::Route::Session { id, server } if *server == here && !s.settings_open() => Some(s.root_of(id).to_owned()),
        _ => None,
    };
    let pinned = pins.is_some();
    let show_hints = crate::tab_hints::shown(s);
    let nav_order = s.nav_order();
    let view = recent_view::current(s);
    let items = sessions.iter().enumerate().map(|(index, info)| {
        let target = Target::on(&here, &info.id, Spot::Row);
        let muted = preferences::muted(&here, &info.id);
        let raw_mark = status(s, &info.id, false);
        let uid = crate::tabs::session_uid(&here, &info.id);
        let shortcut = nav_order.iter().position(|u| u == &uid).filter(|&i| i < 9).map(|i| i + 1);
        let mut item = TabItem::new(info.id.as_str(), crate::tabs::tab_title(&info.title))
            .status(status(s, &info.id, muted))
            .closable(false)
            .marked(s.kernel.slots.get(&crate::session_menu::MENUS).on.as_ref() == Some(&target));
        item = if hide_avatars {
            item.avatar(el().size(16.0, 16.0))
        } else {
            item.avatar(crate::icons::tab_badge(s, p, s.project_for_session(info), &info.title, shortcut, show_hints))
        };
        if consolidated && !muted {
            let st = recent_status(s, &here, &info.id);
            if let Some(meta) = recent_view::meta_label(view, st, info.created, s.recent_page.time(info), *s.now_ms) {
                item = item.subtitle(meta);
            }
        }
        if let Some(field) = crate::session_menu_view::field(s, p, &target, UI_MED) {
            item = item.editor(field);
        }
        let (id, server, count) = (info.id.clone(), here.clone(), sessions.len());
        item.decorate(move |slot, _| {
            let slot = if muted { super::pins::marked_slot(slot, super::pins::muted_mark(p), raw_mark) } else { slot };
            slot.non_closable_tab::<App>().on_secondary_press(move |s: &mut App, press: rune::PointerPress, tx: &mut Tx| {
                open_menu(s, &server, &id, pinned.then_some((index, count)), consolidated, press, tx)
            })
        })
    });
    let mut strip = TabStrip::new(strip_key, TabOrientation::Vertical)
        .words(crate::view::tab_strip::tab_words())
        .tabs(items)
        .active(selected.as_deref())
        .fill(p.bg_deep)
        .close_icon(&ic::XMARK_SMALL)
        .busy_indicator(|k| session_view::style::tab_busy_glyph(k, 14.0))
        .label(t!("sidebar.recent.section.recent"))
        .on_select(|s: &mut App, id, tx| s.open(id, tx))
        .on_hover(|s: &mut App, id, inside, _| {
            if inside {
                s.prefetch_session(id);
            }
        })
        .select_on_drag_start(!session_view::style::touch_controls());
    if let Some(visible) = pins {
        let server = here.as_str().to_owned();
        strip = strip
            .drag_state(&s.gestures.tab_drag, |s: &mut App| &mut s.gestures.tab_drag)
            .on_reorder(move |s: &mut App, r, _| {
                let visible: Vec<String> = visible.iter().map(|id| key(&server, id)).collect();
                s.project_state.recent.reorder(&visible, r.from, r.to);
            });
    }
    let row_h = if consolidated && view.metadata { 48.0 } else { 31.0 };
    let h = rune::memo::layout_with(&format!("{strip_key}-list"), |r| r[3]).unwrap_or(sessions.len() as f32 * row_h);
    col().key(format!("{strip_key}-box")).w_full().h(h).child(strip.build(crate::palette::kit(p)))
}

/// A row's mark, as a side tab's (`useSessionTabAvatarState`); a muted session shows no unread
/// dot (`recentRowUnread`).
fn status(s: &App, id: &str, muted: bool) -> Option<TabItemStatus> {
    Some(match s.tab_status(id)? {
        crate::TabStatus::Busy => TabItemStatus::Busy,
        crate::TabStatus::Attention => TabItemStatus::Attention,
        crate::TabStatus::Unread if muted => return None,
        crate::TabStatus::Unread => TabItemStatus::Unread,
        crate::TabStatus::Failed => TabItemStatus::Failed,
    })
}

/// A row's menu (`RecentSessionTab`'s `menu`).
fn open_menu(
    s: &mut App,
    server: &opencode_kit::servers::ServerKey,
    id: &str,
    pinned: Option<(usize, usize)>,
    consolidated: bool,
    press: rune::PointerPress,
    tx: &mut Tx,
) {
    // `recent-sessions.tsx:1339-1372` (opencode#66576): Rename, Pin/Unpin, Mark read/unread, Mute,
    // Hide, (Move up/down when pinned), ---, Copy >, Export…, ---, Archive, Delete….
    let target = Target::on(server, id, Spot::Row);
    let read = crate::session_menu::lines(s, &target).into_iter().find_map(|line| match line {
        crate::session_menu::Line::Item(i) if matches!(i.id, "read" | "unread") => Some(Item::new(i.id, i.title)),
        _ => None,
    });
    let copy_sub = crate::session_menu::copy::submenu(s, server, id);
    let mut menu = Menu::new(t!("command.category.session"))
        .item(Item::new("rename", t!("common.rename")));
    if consolidated {
        for line in crate::tabs::pins::menu_lines(s, server, id) {
            menu = match line {
                crate::session_menu::Line::Extra(i) => menu.item(i),
                crate::session_menu::Line::Sub { id, title, menu: sub } => menu.submenu(id, title, sub),
                crate::session_menu::Line::Sep => menu.separator(),
                crate::session_menu::Line::Item(i) => menu.item(Item::new(i.id, i.title).enabled(i.enabled).danger(i.danger)),
            };
        }
    } else {
        menu = match pinned {
            None => menu.item(Item::new("pin", t!("sidebar.recent.pin"))),
            Some(_) => menu.item(Item::new("unpin", t!("sidebar.recent.unpin"))),
        };
    }
    let muted = preferences::muted(server, id);
    menu = menu
        .items(read)
        .item(Item::new("mute", if muted { t!("sidebar.recent.unmute") } else { t!("sidebar.recent.mute") }))
        .item(Item::new("hide", t!("sidebar.recent.hide")));
    if !consolidated
        && let Some((at, count)) = pinned
    {
        menu = menu
            .separator()
            .item(Item::new("up", t!("sidebar.recent.moveUp")).enabled(at > 0))
            .item(Item::new("down", t!("sidebar.recent.moveDown")).enabled(at + 1 < count));
    }
    menu = menu.separator();
    if let Some(crate::session_menu::Line::Sub { id: sub_id, title, menu: sub }) = copy_sub {
        menu = menu.submenu(sub_id, title, sub);
    }
    menu = menu
        .item(Item::new("export", format!("{}\u{2026}", t!("common.export"))).enabled(!s.exporting(server, id)))
        .separator()
        .item(Item::new("archive", t!("common.archive")))
        .item(Item::new("delete", format!("{}\u{2026}", t!("common.delete"))).danger(true));
    s.finish_rename(true);
    s.kernel.slots.get_mut(&crate::session_menu::MENUS).on = Some(target.clone());
    tx.instant();
    let pin = key(server.as_str(), id);
    let (srv, sid) = (server.clone(), id.to_owned());
    popup::open(s, menu, press, tx, move |s: &mut App, picked, tx| {
        s.kernel.slots.get_mut(&crate::session_menu::MENUS).on = None;
        tx.instant();
        let Some(picked) = picked else { return };
        if consolidated && crate::tabs::pins::run_menu(s, &srv, &sid, picked) {
            return;
        }
        let visible = || pinned_keys(s);
        match picked {
            "pin" => s.project_state.recent.pin(pin),
            "unpin" => s.project_state.recent.unpin(&pin),
            dir @ ("up" | "down") => {
                let visible = visible();
                s.project_state.recent.shift(&visible, &pin, dir == "up");
            }
            "mute" => preferences::update(s, Kind::Muted, &target.server, &target.id, !muted),
            "hide" => preferences::update(s, Kind::Hidden, &target.server, &target.id, true),
            "archive" => {
                let id = target.id.clone();
                s.archive_session(&id, tx);
            }
            other => crate::session_menu::run(s, &target, other, tx),
        }
    });
}

/// The pins the list shows, in order: the server's, unarchived.
fn pinned_keys(s: &App) -> Vec<String> {
    let here = crate::servers::runtime::active(s).as_str().to_owned();
    partition(s).0.iter().map(|i| key(&here, &i.id)).collect()
}
