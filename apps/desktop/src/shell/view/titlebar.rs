//! The titlebar row (`titlebar.tsx`): Desktop's `titlebar-v2` header across the window's top.
//! Top tabs put the app menu (off macOS), Home, Search sessions, the tab strip and `+` in it;
//! side tabs and the inbox, on Windows, only the app menu and the sidebar toggle. Its gaps, the
//! bands above and below the controls and the stretch after `+` move the window, double-click
//! to maximize it and, on Windows, right-click for the window menu.

use rune_kit::shell::{TabOrientation, TabOverflow, WindowTitlebar};

use super::*;
use crate::layout_state::TabLayout;
use opencode_i18n::t;

/// The tab strip's key, whose width at the last layout the tabs share.
const STRIP: &str = "tab-strip";

/// The width Desktop keeps for the traffic lights before top tabs (`macTrafficLightsBaseWidth`).
const LIGHTS_W: f32 = 74.0;

/// Matched-flight ids for the controls that move between the titlebar and the sidebar.
pub(crate) const NAV_HOME: &str = "shell-nav-home";
pub(crate) const NAV_SEARCH: &str = "shell-nav-search";
pub(crate) const NAV_NEW: &str = "shell-nav-new";
pub(crate) const NAV_AUTOMATIONS: &str = "shell-nav-automations";

/// The row, where this layout and platform keep one.
pub(super) fn titlebar(s: &App, p: &Pal, cx: &mut Cx<App>) -> Option<El> {
    let chrome = chrome();
    if chrome.row_h == 0.0 {
        return None;
    }
    let insets = cx.chrome_insets();
    let top = chrome.layout == TabLayout::Horizontal;
    let mut bar = WindowTitlebar::from_insets(insets)
        .key("titlebar")
        .height(chrome.row_h)
        .top_padding(chrome.row_pad)
        .leading_padding(leading_pad(top, chrome.caption_row, insets.leading, *s.zoom))
        .fill(p.bg_deep)
        // Right to left the row runs from the caption buttons, which stay on the right
        // (`titlebar.tsx:192`), as the traffic lights stay on the left.
        .direction(ui_direction());
    // Desktop's app menu is Windows' and Linux's (`windows-menu.tsx`); a browser has none.
    if !cfg!(any(target_os = "macos", target_arch = "wasm32")) {
        bar = bar.leading(crate::app_menu::button(s, p).motion(GLIDE));
    }
    // A narrow window's row holds one trigger for the tabs' drawer, `px-2` (`titlebar.tsx:755`),
    // and the titlebar items' row 6 after it (`gap-1.5`).
    if chrome.narrow {
        let items = crate::plugin_host::titlebar_items(s, p);
        let mut built = bar.trailing_padding(if items.is_some() { 8.0 } else { 8.0 + 6.0 }).tabs(crate::narrow::tabs::trigger(s, p));
        if let Some(items) = items {
            built = built.trailing(items);
        }
        // Along the foot, its controls sit in the row's top 36, 8 clear of the window's edge.
        if chrome.bottom {
            let row = built.key("titlebar-foot").height(chrome.row_h - 8.0).top_padding(0.0).build(crate::palette::kit(p));
            return Some(col().key("titlebar").w_full().h(chrome.row_h).child(row));
        }
        return Some(built.build(crate::palette::kit(p)));
    }
    if !top {
        return Some(bar.leading(toggle_button(s, p, *s.sidebar_open)).build(crate::palette::kit(p)));
    }
    // `-me-0.5`: the first tab's avatar 10 from Search, past the row's gap and the tab's own inset (#63108).
    // Consolidated navigation searches inline on Home (`titlebar.tsx`, #64668).
    let proto = s.settings.navigation_prototype();
    let search = (!proto).then(|| search_button(p));
    let automations = crate::automations::available(s).then(|| automations_button(s, p));
    let nav = row().key("titlebar-nav").gap(2.0).mr(-2.0).center_y().child(home_button(s, p)).children(search).children(automations);
    let pinned_bar = super::sidebar::pinned_area::horizontal(s, p);
    // Under Settings › Group tabs each project's or server's tabs stand together, the tabs with
    // no project after them (`grouped.horizontal()`, `titlebar.tsx:922`), a 16 × 1 divider
    // between two groups (`tab-strip.tsx:1060-1068`).
    let mut sections = super::tab_strip::Sections::default();
    let visible_indices: Vec<usize> = match crate::tabs::auto_groups::groups(s) {
        Some(groups) => groups.iter().filter(|g| !g.tabs.is_empty()).fold(vec![], |mut shown, g| {
            if !shown.is_empty() {
                let line = el().key("auto-group-divider").size(1.0, 16.0).fill(p.border_muted).a11y_hidden();
                sections.breaks.push(rune_kit::shell::TabBreak::new(format!("divider-{}", g.key), shown.len(), line, 1.0));
            }
            shown.extend(&g.tabs);
            shown
        }),
        None if proto => s.tabs.iter().enumerate().filter(|(_, t)| !crate::tabs::pins::holds(s, t)).map(|(i, _)| i).collect(),
        None => (0..s.tabs.len()).collect(),
    };
    // The strip's width matters only while the tabs can't all have their full 224 pt: below
    // that the tabs share it. Read through `layout_with`, a resize where every tab keeps its
    // full width runs no view; the strip only lays out again.
    // A divider takes its point and one more gap of the row.
    let fits = full_strip_w(visible_indices.len()) + sections.breaks.len() as f32 * 7.0;
    let viewport = cx.layout_with(STRIP, move |r| (r[2] < fits).then_some(r[2])).flatten();
    let strip = super::tab_strip::tab_strip_filtered(s, p, STRIP, TabOrientation::Horizontal, viewport, Some(new_button(p)), &visible_indices, sections);
    let bar = bar.leading(nav);
    let mut bar = match pinned_bar {
        Some(pins) => bar.leading(pins),
        None => bar,
    };
    if let Some(items) = crate::plugin_host::titlebar_items(s, p) {
        let items = match ui_direction() {
            rune::Direction::Rtl => items.ml(12.0),
            _ => items.mr(12.0),
        };
        bar = bar.trailing(items);
    }
    Some(bar.tabs(strip).build(crate::palette::kit(p)))
}

/// Points before the row's first control: `ps-3.5` on Windows, `ps-[sidebar-inset - 2]` beside
/// side tabs (so the menu's glyph lines up with the sidebar's icons), past the traffic lights'
/// 74pt and `pl-2.5` on macOS, and `px-2` elsewhere and in full screen. Linux's row under Rune's
/// own frame (`caption_row`) is Windows'.
fn leading_pad(top: bool, caption_row: bool, lights: f32, zoom: f32) -> f32 {
    match () {
        _ if caption_row && top => 14.0,
        _ if caption_row => INSET_START - 2.0,
        // `WindowTitlebar` already keeps the lights' own width clear; `lights` is in the
        // view's points, the lights' 74 in the window's.
        _ if lights > 0.0 => (LIGHTS_W / zoom - lights).max(0.0) + 10.0,
        _ => 8.0,
    }
}

/// A titlebar icon button (`IconButton` ghost-muted large, `!w-9`): 36 × 28, the hover wash,
/// a press brightening the icon to base ink, and the pressed wash with base ink while `pressed`.
fn nav_button(p: &Pal, key: &str, label: &str, glyph: &'static Icon, pressed: bool) -> El {
    let b = row()
        .key(key.to_owned())
        .role("button")
        .label(label.to_owned())
        .size(36.0, 28.0)
        .radius(6.0)
        .focus_inset(p)
        .center()
        .hover(if pressed { rune::Color::TRANSPARENT } else { p.hover })
        // `ghost-muted`'s `:active` brightens the icon to `icon-base`; its pressed wash loses to the
        // hover rule, whose `:not([data-expanded])` makes it the more specific, so the hover wash stays.
        .press_color(p.icon)
        .hover_motion(INSTANT, INSTANT)
        .child(icon(glyph, 16.0).color(if pressed { p.icon } else { p.icon_muted }));
    if pressed { b.fill(p.pressed) } else { b }
}

fn home_button(s: &App, p: &Pal) -> El {
    let home = s.route == Route::Home && !s.settings_open();
    let (label, glyph) = super::sidebar::pinned_area::home_label(s);
    nav_button(p, "home", label, glyph, home)
        .help(label).help_keys(crate::keymap::Bind::Home.keycaps())
        .matched(NAV_HOME).matched_crop().motion(GLIDE)
        .on_press_tx(|s: &mut App, tx: &mut Tx| {
            s.toggle_home(tx);
        })
}

fn automations_button(s: &App, p: &Pal) -> El {
    let open = *s.route == Route::Automations && !s.settings_open();
    nav_button(p, "automations", t!("automations.title"), &crate::automations::CLOCK, open)
        .help(t!("automations.title"))
        .matched(NAV_AUTOMATIONS).matched_crop().motion(GLIDE)
        .on_press_tx(|s: &mut App, tx: &mut Tx| crate::automations::toggle(s, tx))
}

fn search_button(p: &Pal) -> El {
    nav_button(p, "search", t!("command.session.search"), &ic::MAGNIFYING_GLASS, false)
        .help(t!("command.session.search")).help_keys(crate::keymap::Bind::SessionSearch.keycaps())
        .matched(NAV_SEARCH).matched_crop().motion(GLIDE)
        .on_press_tx(|s: &mut App, tx: &mut Tx| {
            s.close_settings();
            s.open_session_search(tx)
        })
}

/// `+` after the last tab (`RecentlyClosedTabsMenu`'s trigger, 28 × 28): a new session tab, and
/// on a right-click the Recently closed menu.
fn new_button(p: &Pal) -> El {
    row()
        .key("new")
        .role("button")
        .label_static(t!("command.session.new"))
        .size(28.0, 28.0)
        .radius(6.0)
        .focus_inset(p)
        .center()
        .hover(p.hover)
        // As `nav_button`: pressed, the icon brightens and the hover wash stays.
        .press_color(p.icon)
        .hover_motion(INSTANT, INSTANT)
        .help(t!("command.session.new")).help_keys(crate::keymap::Bind::TabNew.keycaps())
        .matched(NAV_NEW).matched_crop().motion(GLIDE)
        .on_press_tx(|s: &mut App, tx: &mut Tx| s.new_session(tx))
        .on_secondary_press(|s: &mut App, press: rune::PointerPress, tx: &mut Tx| crate::recently_closed::open(s, press, tx))
        .child(icon(&ic::PLUS, 16.0).color(p.icon_muted))
}

/// A width at which `tabs` titlebar tabs, the gaps between them and the + after them all fit
/// at their full width: from there up the strip's width changes nothing the view draws. It
/// errs wide (every tab unpinned, a generous gap and +), which only reads the width sooner.
fn full_strip_w(tabs: usize) -> f32 {
    let TabOverflow::ShrinkThenScroll { max_width, .. } = TabOverflow::HORIZONTAL_DEFAULT else { return f32::INFINITY };
    tabs as f32 * (max_width + 8.0) + 96.0
}
