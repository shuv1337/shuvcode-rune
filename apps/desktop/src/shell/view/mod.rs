//! Views: the shell (sidebar + panel), the home page and the session page. Values follow
//! OpenCode Desktop's v2 styles (`packages/app`, `packages/session-ui`, `packages/ui` tokens).

use crate::pickers::PickerHost as _;

use rune::{
    Color, Curve, Cursor, Cx, Drag, Easing, El, Icon, Key, Phase,
    Align, Side, Spring, TextStyle, Tx, col, el, icon, row, spacer, text,
};

use rune_theme_opencode::icons as ic;
use opencode_ui::focus::FocusExt;
use opencode_ui::tab::TabExt;
use opencode_kit::model::{self, SessionInfo, Transcript};
use crate::palette::{Elevate as _, Pal, pal};
use crate::{App, Route, TabStatus};
use crate::pickers::Kind;

mod chrome;
mod composer;
mod draft;
pub(crate) mod draft_location;
mod header;
mod header_fit;
pub(crate) mod home;
pub(crate) mod location;
pub(crate) mod location_card;
pub(crate) mod move_recovery;
pub(crate) mod inbox;
mod narrow;
mod narrow_story;
mod peek_story;
mod rhythm_stories;
mod session;
pub(crate) mod server_unavailable;
pub(crate) mod sidebar;
pub(crate) mod tab_strip;
pub(crate) mod tab_preview;
mod titlebar;

pub(crate) use chrome::*;
pub(crate) use tab_strip::tab_strip;
pub use composer::*;
pub(crate) use draft::*;
pub(crate) use header::*;
pub(crate) use home::{home, home_modifiers};
#[cfg(feature = "stories")]
pub(crate) use home::session_row;
pub(crate) use home::list::{Hold as HomeHold, is_automation};
pub(crate) use session::*;
pub use sidebar::*;

pub(crate) use session_view::composer::PROMPT;
pub(crate) use session_view::style::{CODE, DOCK_SIDE, H2, INSTANT, UI, UI_MED, UI_SMALL};

/// What a layout switch flies its tabs and controls on (`layouts-design.md`'s GLIDE).
pub(crate) const GLIDE: Spring = rune::tok::motion::GLIDE;

/// The window, at the app's zoom (Desktop's `setZoomFactor`): Rune lays the view out in the
/// window's points over the zoom at its scale times the zoom (`Cx::zoom`), as Chromium folds a
/// page zoom into the device scale, so every size, border, icon and motion is the zoomed one.
pub fn app(s: &App, cx: &mut Cx<App>) -> El {
    s.bump_view_runs();
    cx.zoom(*s.zoom);
    session_view::scroller::set_device_scale(cx.env().scale);
    // A system contrast theme paints the app in its colours alone, whatever theme is picked.
    match cx.system_contrast() {
        Some(c) => crate::themes::set_active(crate::themes::contrast(&c)),
        None => crate::themes::set_active(&s.settings.theme),
    }
    // macOS's Increase Contrast keeps the theme and draws its highlights as edges.
    crate::themes::set_raised_contrast(cx.system_increased_contrast());
    s.appearance_state.motion.note_system(cx.system_reduced_motion());
    let p = pal(s.dark(cx));
    // Pages draw in the app's scheme, as Desktop's follow its `nativeTheme.themeSource`.
    s.workspace_panes.browser.host.set_color_scheme(s.dark(cx));
    // Desktop's `(max-width: 767px)`: the shell's width, read once it crosses or while narrow.
    let width = cx.layout_with("zoom", move |r| r[2].min(crate::narrow::machine::BREAKPOINT));
    let was_narrow = s.layout.narrow.is_narrow();
    let narrow = width.is_some_and(|w| s.layout.narrow.measure(w)) && s.fixture_run.stage.is_none();
    if was_narrow != narrow {
        let reading = narrow.then(|| s.active_session_id()).flatten()
            .filter(|id| !s.timeline_fx.ends.due(id) && s.displayed_session(id) == Some(*id) && !s.settings_open())
            .and_then(|id| s.timeline_fx.ends.recall.place(id).map(|(key, y)| (id.to_owned(), key, y)));
        *s.layout.narrow.reading.borrow_mut() = reading;
    }
    // A narrow window keeps top tabs' titlebar row in every layout, its tabs in a drawer.
    let layout = if narrow { crate::layout_state::TabLayout::Horizontal } else { s.tab_layout() };
    // A phone's notch, status bar and home indicator (a browser's safe area; zero natively).
    let safe = cx.safe_area().map(|inset| inset / *s.zoom);
    session_view::style::set_safe_area(safe);
    set_chrome(cx.chrome_insets(), *s.zoom, cx.env().scale, layout, narrow, s.settings.mobile_titlebar == crate::settings::TitlebarPosition::Bottom, safe[0] > 0.0);
    session_view::style::set_narrow(narrow);
    session_view::style::set_pointer(cx.hover_none(), cx.touch_controls());
    // Timeline width sets the session column for everything this frame builds.
    session_view::style::set_content_w(session_view::TimelineHost::content_width(s));
    // The caption buttons line up with the titlebar row at every zoom; on macOS the traffic
    // lights move to the layout's titlebar.
    if cfg!(target_os = "macos") {
        cx.set_titlebar(mac_titlebar(layout, *s.zoom, chrome().bottom));
    } else {
        cx.titlebar_height(titlebar_height(*s.zoom));
    }
    // A system contrast theme draws them in its text colour, as Desktop's `window-text` (opencode#65324).
    cx.caption_color(if p.forced { p.text } else { caption_color(p.bg_deep) });
    // Text and fields left uncoloured take the theme's text, and focusable controls without a
    // ring of their own draw the shared one (`opencode_ui::focus`, base.css `:focus-visible`).
    cx.foreground(p.text);
    cx.focus_ring(opencode_ui::focus::WIDTH, opencode_ui::focus::OFFSET, opencode_ui::focus::color(p));
    // Every button, link and tab is a Tab stop in document order, and dialogs open focused, as
    // in Desktop's DOM.
    cx.web_focus();
    // The debug bar's FOCUS: every control shows its focus ring.
    if s.plugins.force_focus {
        cx.force_focus_rings();
    }
    // Tooltips are Desktop's compact `Tooltip` (`tooltip.css`), placed and timed as its Kobalte.
    cx.help_look(rune_kit::TooltipStyle::from_kit(crate::palette::kit(p), rune_kit::TooltipAppearance::Compact).help_look());
    // A quote's source stays lit in the timeline while its comment is written.
    cx.mark_texts(crate::quote::marks(s));
    crate::icons::prepare(s, cx);
    crate::diagram::prepare(s);
    crate::files::link_tip::prepare(s);
    let mut inner = match *s.fixture_run.stage {
        Some(st) => {
            cx.reduced_motion(*s.appearance_state.reduced_motion);
            crate::stories::page(s, p, st)
        }
        None => shell(s, cx),
    };
    inner = inner.children(s.kernel.overlay_dispatcher.render_all(s, cx));
    let root = el().key("zoom");
    // The app keeps clear of the safe area and the window's colour runs under it to the screen's
    // edge; the shell's surfaces reach under the side and bottom insets themselves (`shell`).
    let [top, right, bottom, left] = safe;
    let root = if top + right + bottom + left > 0.0 { root.pt(top).pr(right).pb(bottom).pl(left).fill(p.bg_deep) } else { root };
    // Arabic, Hebrew, Persian, Urdu… mirror the whole window, as Desktop's `dir` on the
    // document root does (`language.tsx:397`); the caption buttons Rune draws stay put.
    let root = root.dir(ui_direction());
    root.on_key_up(|s: &mut App, ev, tx| crate::dictation::key_up(s, ev, tx))
        .on_window_focus(|s: &mut App, focused, tx| {
            s.plugins.window_blurred.set(!focused);
            if !focused {
                #[cfg(not(target_arch = "wasm32"))]
                s.workspace_panes.browser.host.close_extension_popup();
                s.clear_app_link_reviews();
                s.clear_preview(tx);
                crate::tab_hints::window_left(s, tx);
            }
            if focused { crate::launch::network_available(s, tx); }
            // The coordinator's `tip` opens on the window's `focus`.
            crate::tips::on_focus(s, focused);
        })
        // A resize across the breakpoint reaches the drawers in the frame it lands.
        .on_layout_tx(|s: &mut App, rect, tx: &mut Tx| {
            #[cfg(not(target_arch = "wasm32"))]
            let closed_popup = s.workspace_panes.browser.host.observe_window_size(rect[2], rect[3]);
            #[cfg(target_arch = "wasm32")]
            let closed_popup = false;
            let flipped = s.observe_app_link_window_rect(rect, tx);
            if !crate::narrow::settle(s, tx) && !flipped && !closed_popup { tx.live_only(); }
        })
        .on_magnify(|s: &mut App, ev, tx| s.pinch(ev, tx))
        // A text field's menu shows and flips Settings › General › Spellcheck, as Desktop's
        // context menu does (`spellcheck.ts`).
        .on_spellcheck(s.settings.spellcheck, |s: &mut App, on, _| s.change_settings(|x| x.spellcheck = on))
        // Off macOS it also picks the dictionaries (Spellcheck Languages).
        .on_spellcheck_languages(|s: &mut App, tags, _| s.change_settings(|x| x.spellcheck_languages = tags))
        .w_full()
        .h_full()
        .center()
        // Hovers land and leave at once, like Desktop.
        .hover_motion_within(INSTANT, INSTANT)
        .child(crate::glitch::wrap(s, el().key("zoomed").w_full().h_full().child(inner)))
        // Stays outside `OverlayDispatcher` and `glitch::wrap(zoomed(inner))`: the developer knobs
        // panel must draw above the paused glitch scrubber and outside the zoomed shell subtree.
        .child(crate::knobs::layer(s))
}

fn shell(s: &App, cx: &mut Cx<App>) -> El {
    let p = pal(s.dark(cx));
    cx.time_scale(crate::knobs::time_scale(s));
    cx.reduced_motion(*s.appearance_state.reduced_motion);
    cx.time_scale(crate::knobs::time_scale(s));
    let w = *s.sidebar_w;
    let side_tabs = s.shows_sidebar();
    // A narrow window's panel runs edge to edge, 1 pt clear for its outline (`--shell-inline-inset`).
    let inset = if s.narrow() { 1.0 } else { 8.0 };
    let open = *s.sidebar_open && side_tabs;
    // The side region's width comes from the row's while it's open or sliding shut (it keeps the
    // last as the far end of a close's travel); shut, nothing reads it, so a resize runs no view.
    // Opening reads it fresh, in the frame it opens.
    let win_w = if s.side_mounted() {
        cx.layout_size("zoom").map_or(0.0, |[w, _]| w)
    } else {
        0.0
    };
    let need_panel_sz = (!s.settings_open() && matches!(*s.route, Route::Draft(_) | Route::Home | Route::Automations)) || s.composer_ext.attach.drop.present();
    let panel_sz = if need_panel_sz {
        cx.layout_size("panel").map_or((0.0, 0.0), |[w, h]| (w, h))
    } else {
        (0.0, 0.0)
    };
    let settings_sz = if s.settings_open() { cx.layout_size("panel").map_or((0.0, 0.0), |[w, h]| (w, h)) } else { (0.0, 0.0) };
    // The bottom pane's reserve and the pane itself read the side region's tabs, which ask every
    // panel's plugin: once per view.
    let side_reg = (!s.settings_open() && s.bottom_has_candidate()).then(|| s.resolve_side_region()).flatten();
    let bottom_reserve = crate::panels::view::bottom_reserve_height(s, side_reg.as_deref());
    let dock = crate::panels::view::bottom_dock(s, side_reg.as_deref(), p, cx);
    // On a phone the shell's surfaces run under the side and bottom insets to the screen's edge
    // and keep their content clear of them, as the window's colour does under the status bar.
    // With a sidebar, a side region or a bottom pane out, the shell stays inside them.
    let bleed = cfg!(target_arch = "wasm32") && s.sidebar_shown_w() == 0.0 && win_w == 0.0 && dock.is_none();
    let [_, inset_r, inset_b, inset_l] = if bleed { session_view::style::safe_area() } else { [0.0; 4] };
    // A layout switch moves the panel, the sidebar and every tab on GLIDE, with the panel on top:
    // the tabs slide under its edges between the titlebar and the sidebar rather than over the
    // page. The titlebar row comes and goes at once, and below the panel while it does.
    let switching = s.switching_layout();
    // The titlebar rebuilds only when what it shows changed: the `Tracked` state it reads (the
    // tabs, the route, the sessions behind them, menus, the drag), and what isn't tracked, as
    // inputs: the palette, the chrome, the locale, the zoom and whether a tab can be torn out.
    let bar_inputs = (
        (p.theme, p.generation, p.dark),
        format!("{:?} {:?}", chrome(), cx.chrome_insets()),
        opencode_i18n::locale().id(),
        s.zoom.to_bits(),
        *s.sidebar_open,
        s.can_tear(),
        crate::tab_hints::shown(s),
        crate::plugin_host::surfaces_version(s),
    );
    let band = (chrome().row_h > 0.0)
        .then(|| rune::region_with("titlebar", bar_inputs, || titlebar::titlebar(s, p, cx).unwrap_or_else(el)))
        .map(|bar| {
            // Settings › Bottom navigation moves a narrow window's row to its foot (`order-last`).
            let band = el().key("titlebar-band").role("banner").h(chrome().row_h).pl(inset_l).pr(inset_r).transition(rune::Transition::none()).child(bar);
            if chrome().bottom { band.pin(Some(0.0), None, Some(0.0), Some(0.0)) } else { band.pin(Some(0.0), Some(0.0), Some(0.0), None) }
        });
    let (band_below, band) = if switching { (band, None) } else { (None, band) };
    // Desktop's `<main>` has `contain: content` (`shell.tsx`), so Chromium rounds its origin to a
    // device pixel on its own and everything in it against that: at 1.875x, 44 pt is 82.5 px and
    // its content sits on the pixel below where its own position would round. What Desktop puts
    // in `<main>` snaps against it here too.
    let main = [s.sidebar_shown_w(), chrome().main_top()];
    // `session/screen.tsx`'s frame: bg-base, 10 pt corners, `--v2-elevation-raised`. Its edge rides
    // the side region's flight by layout in Desktop too, on whole device pixels. Under a phone's
    // side insets the frame reaches the screen's edge and the page (`panel`) sits inside them.
    let surface = |frame: El| {
        let frame = frame.grow(1.0).shrink().ml(inset).mt(chrome().top()).mr(inset).mb(8.0 + bottom_reserve + chrome().foot())
            .radius(10.0).fill(p.bg_base).elevate(p.raised).clip_soft(0.0);
        if switching { frame.motion(GLIDE).raise() } else { frame.motion(crate::panels::view::SIDE_SPRING).motion_snaps() }
    };
    let page = col()
        .key("panel")
        .role("main")
        .on_background_press(|s: &mut App| {
            let side = s.kernel.slots.get_mut(&crate::panels::SIDE);
            side.focus.blur();
            side.menu.close();
        })
        // Files dropped anywhere on the page attach, as in Desktop; a timeline image
        // dragged out of the page attaches only on the composer.
        .on_drop_files(crate::dropzone::drop)
        .accepts_files(|path| !session_view::images::drag::is_own(path))
        .on_drop_hover(crate::dropzone::hover)
        .child(if s.settings_open() { crate::settings::page(s, p, settings_sz) } else { match &*s.route {
            Route::Home => home(s, p, [panel_sz.0, panel_sz.1], cx.layout_size("zoom").unwrap_or([0.0, 0.0])),
            Route::Automations => crate::automations::view::page(s, p, [panel_sz.0, panel_sz.1]),
            Route::Session { id, .. } => session_page(s, p, id),
            Route::Draft(_) => draft_page(s, p, panel_sz.0),
        } })
        .child(crate::dropzone::overlay(s, p, panel_sz));
    let panel = if inset_l + inset_r > 0.0 {
        surface(col().key("panel-frame").pl(inset_l).pr(inset_r)).child(page.grow(1.0).w_full().min_h(0.0))
    } else {
        surface(page)
    };
    let mut root = row()
        .key("window")
        .role("window")
        .fill(p.bg_deep)
        .w_full()
        .h_full();
    // Desktop keeps its route history in memory (`titlebar.tsx:206-213`, UP-116 #65129), so mouse
    // Back/Forward side buttons step it on release; wasm32 lets the host browser traverse its own.
    #[cfg(not(target_arch = "wasm32"))]
    {
        root = root
            .pointer_buttons([rune::PointerButton::Back, rune::PointerButton::Forward])
            .on_background_press_tx(|s: &mut App, tx: &mut Tx| match tx.button() {
                rune::PointerButton::Back => { s.navigate(false, tx); }
                rune::PointerButton::Forward => { s.navigate(true, tx); }
                _ => {}
            });
    }
    root = root
        // The hover peek's zones only listen for the pointer, which reaches them under anything:
        // under the sidebar and the page, a right-click lands on the row under the pointer
        // rather than on them, and its menu opens.
        .children(sidebar::peek(s))
        .child(sidebar_region(s, p))
        .children(band_below)
        // The empty inset above the panels moves the window, as the titlebar does; on Windows
        // it's the titlebar row itself.
        .child(el().key("drag-inset").pin(Some(0.0), Some(0.0), Some(chrome().caption_w), None).h(chrome().top()).window_drag().unmirrored())
        // The titlebar row before the page, as in Desktop's DOM, so Tab reaches it first.
        .children(band)
        // Toggle review comes next, before the page (`screen.tsx`: its `z-30` row ahead of the
        // chat panel), and paints over the page and the panels after it.
        // It is drawn at the window's level over the session header's slot, and Desktop's is inside the
        // header (`message-timeline.tsx:644`), inside the timeline's scroll bar hover target: the
        // pointer on it shows the timeline's thumb, as the pointer on the header does.
        .children(crate::panels::view::side_toggle(s, p).map(|toggle| {
            let toggle = match s.active_session_id() {
                Some(id) => rune_kit::ScrollBar::scroll_bar_hover(toggle, &format!("scroll-{id}")),
                None => toggle,
            };
            toggle.paint_order(1).paint_contained(main)
        }))
        // `paint_contained` establishes its children's pixel grid. The painted frame is
        // itself inside <main> (shell.tsx:304), not the element establishing containment.
        // Keep its ring, shadow and clip on the same grid as the content they surround.
        .child(row().key("main-content").grow(1.0).min_w(0.0).h_full().paint_contained(main).child(panel))
        .child(crate::panels::view::side_region(s, p, win_w, bottom_reserve, main, cx));
    if let Some(dock) = dock {
        root = root.child(
            dock.pin(Some(s.sidebar_shown_w()), None, Some(0.0), Some(0.0))
                .motion(crate::panels::view::SIDE_SPRING)
                .paint_contained(main),
        );
    }
    // A plugin's cover over a tab whose server isn't ready (`PluginServerCover`): the whole main
    // area from under the titlebar row (`<main>`, not the panels' inset in it), the page and its
    // panels mounted under it.
    if let Some(cover) = (!s.settings_open()).then(|| crate::plugin_host::server_cover(s, p)).flatten() {
        root = root.child(cover.pin(Some(s.sidebar_shown_w()), Some(chrome().main_top()), Some(0.0), Some(chrome().foot())).paint_contained(main));
    }
    if open {
        root = root.child(resize_handle(w));
    }
    if let Some(handle) = crate::panels::view::resize_handle(s, p, win_w) {
        root = root.child(handle);
    }
    // Without a titlebar row, the toggle rides the sidebar header's line (`sidebar-toggle.tsx`).
    if side_tabs && chrome().sidebar_header() {
        root = root.child(sidebar_toggle(s, p, *s.sidebar_open, w));
    }
    // Plugins' `window.bottom` slot (the debug bar) runs across the window's foot, under the
    // panels, taking its own room (`shell.tsx`'s last `PluginSlot`).
    let sid = match &*s.route {
        Route::Session { id, .. } => id.as_str(),
        _ => "",
    };
    let bottom = crate::plugin_host::slot(s, p, "window.bottom", sid);
    // Its own ground runs under the insets; its controls stay clear of them.
    let touch_bar = composer::touch::bar(s, p).map(|bar| bar.pl(8.0 + inset_l).pr(8.0 + inset_r).min_h(57.0 + inset_b).h(57.0 + inset_b).pb(inset_b));
    let foot_clear = if touch_bar.is_none() { inset_b } else { 0.0 };
    // One shape whether the foot holds anything or not: a plugin filling its slot at idle (after
    // the first frames) mustn't move the window's tree under a new path, which mounts every
    // element afresh and puts each scroll view back on its start or end.
    let window = col()
        .key("window-column")
        .w_full()
        .h_full()
        .fill(p.bg_deep)
        .child(el().key("window-main").w_full().grow(1.0).min_h(0.0).child(root))
        .children((!bottom.is_empty()).then(|| col().key("plugin-window-bottom").w_full().children(bottom)))
        .children(touch_bar)
        .pb(foot_clear);
    // The web app's `#root` is `p-px` (`packages/app/index.html`; Desktop's renderer isn't): the
    // whole shell sits 1 pt in from the page's edges, and the panels' shadows end there, as `<main>`'s
    // overflow clips them. Sheets and menus, fixed to the page, don't.
    if !cfg!(target_arch = "wasm32") {
        return window;
    }
    let web_shell = el().key("web-shell").clip_soft(0.0).child(window);
    // Inside the safe area, or out under its side and bottom insets to 1 pt from the screen's edge.
    let web_shell = if inset_l + inset_r + inset_b > 0.0 { web_shell.pin(Some(1.0 - inset_l), Some(1.0), Some(1.0 - inset_r), Some(1.0 - inset_b)) } else { web_shell.w_full().h_full() };
    el().key("web-root").w_full().h_full().padding(1.0).fill(p.bg_deep).child(web_shell)
}

// ---------------------------------------------------------------------------------------------
// Pieces

/// CSS `text-left` in a right-to-left window: the left edge, which a right-to-left text's
/// `End` is. `Start` left to right.
pub(crate) fn text_left() -> Align {
    match opencode_i18n::direction() {
        opencode_i18n::Direction::Ltr => Align::Start,
        opencode_i18n::Direction::Rtl => Align::End,
    }
}

/// The window's layout direction: the language's (`language.direction()`).
pub(crate) fn ui_direction() -> rune::Direction {
    match opencode_i18n::direction() {
        opencode_i18n::Direction::Ltr => rune::Direction::Ltr,
        opencode_i18n::Direction::Rtl => rune::Direction::Rtl,
    }
}

pub(crate) fn title_of(s: &SessionInfo) -> String {
    if s.title.is_empty() { opencode_i18n::t!("rune.session.untitled").into() } else { s.title.clone() }
}

pub(crate) fn project_or_dir(project: Option<&model::Project>, dir: &str) -> String {
    project.map(|p| p.name.clone()).unwrap_or_else(|| opencode_kit::path::basename(dir).to_owned())
}

/// The sidebar as a region: a keystroke elsewhere builds none of it. Everything it shows is
/// `Tracked` state it reads, but the palette, the chrome, the language (its direction) and the
/// plugins' sidebar items, which are its inputs.
fn sidebar_region(s: &App, p: &Pal) -> El {
    let inputs = ((p.theme, p.generation, p.dark), format!("{:?}", chrome()), opencode_i18n::locale().id(), crate::plugin_host::surfaces_version(s));
    rune::region_with("sidebar", inputs, || sidebar(s, p))
}
