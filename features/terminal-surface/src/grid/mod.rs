//! High-throughput Rune terminal grid renderer (`120 fps`, per-row `RowId` `memo` + `GlyphCache`,
//! box-drawing vector shapes, selection, interactive scrollback scrollbar, and phase-locked cursor).

mod cell;
mod edge;
mod pointer;
mod quote;
mod rows;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use rune::{
    Cursor, El, Family, LineCap, PointerButton, Shape, Span, TextStyle, canvas, col, el, rich,
    row, spacer, text,
};
use rune_knobs::Knobs;
use opencode_ui::palette::Pal;
use opencode_ui::style::{UI, UI_SMALL};
use terminal::{CursorShape, TerminalSnapshot};

pub(crate) use self::pointer::poll_edge_scroll;
pub use self::quote::{QuotePlace, QuoteSource};
use self::pointer::{ViewportInput, attach_text_drag, attach_viewport_handlers, render_quote_bar, render_scrollbar};
use self::rows::{build_rows, cell_fg_color};
use crate::mount::TextDrag;
use crate::{HoverLink, TERM_FONT, TERM_SIZE, TerminalSurfaceHost, TerminalTab, find, input, links};

/// The terminal's pane, which owns its keys while the screen in it has focus.
pub const VIEWPORT_KEY: &str = "terminal-viewport";
/// The terminal's screen (`rune::grid`): its Tab stop, the text a reader reads, and the key that
/// tells the terminal it has the keyboard.
pub const SCREEN_KEY: &str = "terminal-rows";

/// Desktop's panel strip (`--tabs-bar-height`).
pub const BAR_H: f32 = 48.0;
/// The terminal's inset in Desktop's panel (`px-[14px] py-3`).
pub const PAD_X: f32 = 14.0;
pub const PAD_Y: f32 = 12.0;
pub(super) const SCROLLBAR_W: f32 = 8.0;

/// Desktop's `terminal` icon (`packages/ui/src/icons/catalog.ts:76-78`): 16x16 prompt in a box.
pub static TERMINAL_ICON: rune::Icon = rune::Icon {
    name: "terminal-tab",
    view: 16.0,
    body: r#"<path d="M4.5 11.5L7.5 8.5L4.5 5.5M11.5 11.5H8.5" stroke-linecap="round" stroke-linejoin="round" stroke="currentColor" fill="none"/><path d="M1.5 14.25V1.75C1.5 1.61193 1.61193 1.5 1.75 1.5H14.25C14.3881 1.5 14.5 1.61193 14.5 1.75V14.25C14.5 14.3881 14.3881 14.5 14.25 14.5H1.75C1.61193 14.5 1.5 14.3881 1.5 14.25Z" stroke-linecap="round" stroke="currentColor" fill="none"/>"#,
};

type MetricCache = HashMap<(u8, u32, u64), (f32, f32, f32)>;

thread_local! {
    static METRIC_CACHE: RefCell<MetricCache> = RefCell::new(HashMap::new());
}

thread_local! {
    /// Settings › Appearance › Terminal Font's installed family, before the default face, as a
    /// family list; `None` for one of the knob's faces.
    static CUSTOM: Cell<Option<&'static str>> = const { Cell::new(None) };
}

fn css_family(name: &str) -> String {
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return name.to_owned();
    }
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Puts installed family `name` (empty for none) before the terminal's default face. Each
/// distinct name is kept for the app's life, as a text style names its family.
pub fn set_custom_font(name: &str) {
    thread_local! {
        static KEPT: RefCell<HashMap<String, &'static str>> = RefCell::new(HashMap::new());
    }
    let list = (!name.is_empty()).then(|| {
        KEPT.with_borrow_mut(|kept| {
            *kept.entry(name.to_owned()).or_insert_with(|| {
                Box::leak(format!("{}, OpenMono, OpenMono Icons", css_family(name)).into_boxed_str())
            })
        })
    });
    CUSTOM.set(list);
}

pub fn terminal_font_style(knobs: &Knobs) -> (u8, TextStyle) {
    let custom = CUSTOM.get();
    // A typed family measures apart from the knob's faces.
    let choice = if custom.is_some() { 200 } else { knobs.get(&TERM_FONT).round() as u8 };
    let size = knobs.get(&TERM_SIZE).clamp(10.0, 22.0);
    // ghostty-web's cell: the face's ascent and descent, 18 px at Desktop's 14 px.
    let lh = (size * 1.3).round();
    let family = match choice {
        _ if custom.is_some() => Family::Named(custom.unwrap_or_default()),
        // By name, so the code font (the mono slot) doesn't change the terminal's.
        1 => Family::Named("IBM Plex Mono, OpenMono, OpenMono Icons"),
        2 => Family::SystemMono,
        3 => Family::Named("OpenMono, OpenMono Icons"),
        _ => Family::Named("CommitMono, OpenMono, OpenMono Icons"),
    };
    let mut st = TextStyle::new(size, 400.0).lh(lh);
    st.family = family;
    // Disable ligatures/contextual alternates that could shift cell widths on grid runs.
    st.features = "'calt' 0, 'liga' 0";
    (choice, st)
}

pub fn cell_metrics(choice: u8, style: TextStyle) -> (f32, f32) {
    cell_metrics_at(choice, style, 1.0)
}

/// The cell at `scale` device pixels a point, as ghostty-web measures it (`renderer.ts`
/// `measureFont`): the advance of "M" and the face's ascent plus descent, each rounded to whole
/// device pixels, so the grid stays integral. At 14 pt on a 1x screen JetBrains Mono's 8.4 pt
/// advance makes an 8 pt cell, 8.67 at 1.5x.
pub fn cell_metrics_at(choice: u8, style: TextStyle, scale: f32) -> (f32, f32) {
    let (cw, ch, _) = measure(choice, style, scale);
    (cw, ch)
}

/// The spacing that sets the face's glyphs one cell apart: the cell less the face's advance.
pub fn cell_tracking(choice: u8, style: TextStyle, scale: f32) -> f32 {
    let (cw, _, advance) = measure(choice, style, scale);
    cw - advance
}

fn measure(choice: u8, style: TextStyle, scale: f32) -> (f32, f32, f32) {
    // A font switch (Settings › Appearance) can change what a style's family resolves to.
    // A typed family measures apart from the knob's faces and from another typed family.
    let family = match style.family {
        Family::Named(list) => list.as_ptr() as u64,
        _ => 0,
    };
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let key = (choice, style.size.to_bits() ^ scale.to_bits().rotate_left(16), rune::text::font_epoch() ^ family);
    METRIC_CACHE.with(|c| {
        if let Some(&m) = c.borrow().get(&key) {
            return m;
        }
        let style = style.tracking(0.0);
        let advance = rune::text::advance("MMMMMMMMMM", style) / 10.0;
        let advance = if advance > 1.0 { advance } else { (style.size * 0.6).max(6.0) };
        let cw = (advance * scale).round().max(1.0) / scale;
        // OpenMono's font box (hhea 965 up, 220 down at 1000 UPM, `opencode#64797`), each side
        // rounded as the canvas reports it; another face keeps its line height.
        let ch = if choice == 3 {
            let px = style.size * scale;
            ((px * 0.965).round() + (px * 0.22).round()).max(1.0) / scale
        } else {
            style.line_px().max(style.size.ceil() + 2.0)
        };
        let m = (cw, ch, advance);
        c.borrow_mut().insert(key, m);
        m
    })
}

/// A terminal's view in its pane (Desktop's `TerminalPanel`): the selected terminal `tab_id` (the
/// workspace's active one without it) inset 14 by 12, or the notice while the session's terminals
/// load or failed to. Its tab lives in the side region's shared strip.
#[allow(clippy::too_many_arguments)]
pub fn render_terminal_pane<H: TerminalSurfaceHost + 'static>(
    s: &H,
    p: &Pal,
    ws_key: &str,
    tab_id: Option<&str>,
    pane_size: (f32, f32),
    env_scale: f32,
    focused: bool,
    shown: bool,
) -> El {
    let (font_choice, style) = terminal_font_style(s.knobs());
    // ghostty-web's whole-device-pixel cell, the glyphs spaced one cell apart. `env_scale` is the
    // view's, at the app's zoom.
    let scale = env_scale;
    let (cell_w, cell_h) = cell_metrics_at(font_choice, style, scale);
    let style = style.tracking(cell_tracking(font_choice, style, scale)).lh(cell_h);
    let (pane_w, pane_h) = (pane_size.0.max(200.0), pane_size.1.max(100.0));
    let body_w = (pane_w - PAD_X * 2.0).max(cell_w * 10.0);
    let body_h = (pane_h - PAD_Y * 2.0).max(cell_h * 2.0);
    let fit_cols = ((body_w / cell_w).floor() as u16).clamp(10, 500);
    let fit_rows = ((body_h / cell_h).floor() as u16).clamp(2, 240);
    let cw_px = ((cell_w * scale).round() as u32).max(1);
    let ch_px = ((cell_h * scale).round() as u32).max(1);

    let pane = s.terminal();
    let Some(ws) = pane.workspaces.get(ws_key) else {
        return col()
            .key("terminal-empty")
            .w_full()
            .h_full()
            .fill(p.bg_base)
            .child(notice(p, opencode_i18n::t!("plugin.terminal.loading"), None));
    };
    if ws.tabs.is_empty()
        && let Some(err) = &ws.failure
    {
        return col()
            .key("terminal-failed")
            .w_full()
            .h_full()
            .fill(p.bg_base)
            .child(notice(p, opencode_i18n::t!("plugin.terminal.list.failed"), Some(err)));
    }

    let Some(tab) = tab_id
        .or(ws.active.as_deref())
        .and_then(|id| ws.tabs.iter().find(|t| t.id == id))
        .or_else(|| ws.tabs.first())
    else {
        return col().key("terminal-no-active").w_full().h_full().fill(p.bg_base);
    };

    // Sync grid fit when the pane's layout dimensions change.
    tab.ensure_fit(fit_cols, fit_rows, cw_px, ch_px);

    // A pane kept mounted out of sight (a closed region keeps its card for the slide) draws the
    // last screen without reading a new one, so its terminal builds none (`publish.rs`).
    let snap = if shown { tab.session.snapshot() } else { tab.session.peek() };
    let viewport = render_viewport(
        s,
        p,
        ws_key,
        tab,
        &snap,
        style,
        font_choice,
        cell_w,
        cell_h,
        scale,
        fit_cols,
        body_h,
        focused,
    );

    // A stopped attachment keeps its last screen under Desktop's notice (`text-12-regular
    // text-text-danger-base`, `px-6 py-3` on the terminal's background).
    let failure = match tab.session.status() {
        terminal::SessionStatus::Failed(why) => Some(why),
        _ => None,
    };
    let find_bar = find::bar::<H>(p, ws_key, tab, &snap);
    let bell = pane.bell.is_ringing(s.clock());
    pane_shell(p, viewport, find_bar, failure, bell)
}

fn pane_shell(p: &Pal, viewport: El, find_bar: Option<El>, failure: Option<String>, bell: bool) -> El {
    let body = col()
        .key("terminal-body")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(viewport)
        .children(find_bar)
        .children(failure.map(|why| {
            el().key("terminal-failure")
                .absolute(0.0, 0.0)
                .w_full()
                .px(24.0)
                .py(12.0)
                .fill(p.bg_base)
                .inert()
                // Desktop's `text-text-danger-base` names no token, so the notice keeps the pane's
                // strong ink.
                .child(text(why).font(UI_SMALL).color(p.text))
        }));

    // A bell flashes the pane's top edge in the accent.
    col()
        .key("terminal-pane")
        .role("region")
        .label_static(opencode_i18n::t!("plugin.terminal.tab.title"))
        .w_full()
        .h_full()
        .fill(p.bg_base)
        .instant_changes()
        .children(bell.then(|| el().key("terminal-bell").absolute(0.0, 0.0).w_full().h(1.0).fill(p.accent)))
        .child(body)
}

/// Desktop's centred panel notice: a 13 px strong line over a 12 px weak one.
fn notice(p: &Pal, title: &str, detail: Option<&str>) -> El {
    col()
        .key("terminal-notice")
        .w_full()
        .grow(1.0)
        .center()
        .gap(4.0)
        .px(16.0)
        .child(text(title.to_owned()).font(UI).color(if detail.is_some() { p.text } else { p.faint }))
        .children(detail.map(|d| text(d.to_owned()).font(UI_SMALL).color(p.faint)))
}

#[allow(clippy::too_many_arguments)]
fn render_viewport<H: TerminalSurfaceHost + 'static>(
    s: &H,
    p: &Pal,
    ws_key: &str,
    tab: &TerminalTab,
    snap: &TerminalSnapshot,
    style: TextStyle,
    font_choice: u8,
    cell_w: f32,
    cell_h: f32,
    scale: f32,
    fit_cols: u16,
    body_h: f32,
    focused: bool,
) -> El {
    let style_key = (font_choice, style.size.to_bits(), cell_w.to_bits(), cell_h.to_bits(), scale.to_bits());
    let avail_h = body_h.max(cell_h);
    let slot_rows = (((avail_h / cell_h).ceil() as usize) + 1).max(snap.screen_rows.len());
    let clip_w = f32::from(fit_cols) * cell_w;
    let grid_h = (slot_rows as f32) * cell_h;

    let (hovered, follow_mod) = if snap.mouse_tracking {
        (Vec::new(), false)
    } else {
        match tab.hover {
            HoverLink::None => (Vec::new(), false),
            HoverLink::Plain { col, row } => (links::span(snap, col, row), false),
            HoverLink::Followable { col, row } => (links::span(snap, col, row), true),
        }
    };
    let follows = !hovered.is_empty() && follow_mod;

    let cols = snap.screen_rows.first().map_or(fit_cols, |r| r.cells.len() as u16);
    let rows = build_rows(s.terminal(), &tab.id, snap, p, style, style_key, cell_w, cell_h, scale, slot_rows);
    let input = ViewportInput {
        ws_key: ws_key.to_owned(),
        tab_id: tab.id.clone(),
        cell_w,
        cell_h,
        cols_cap: snap.cols.max(1),
        rows_cap: snap.rows.max(1),
        reports: snap.mouse_tracking,
        origin: (PAD_X, PAD_Y),
    };
    let text_input = ViewportInput { origin: (0.0, 0.0), ..input.clone() };
    let text_drag = |text: El| attach_text_drag::<H>(text, text_input);
    let mut canvas_layer = build_canvas_layer(s, p, tab, snap, style, rows, &hovered, cols, cell_w, cell_h, clip_w, avail_h, grid_h, focused, text_drag);

    // Quote: a settled selection offers to go into the composer, under its last line (above
    // when that's the screen's bottom), as Desktop's `TerminalQuote` does.
    if matches!(tab.drag, TextDrag::Idle)
        && let Some(selected) = tab.session.selected_text()
        && s.can_quote_terminal_selection(&selected)
        && let Some((left, last)) = selection_corner(snap)
    {
        let below = f32::from(last + 1) * cell_h + 6.0;
        let y = if below + 28.0 > avail_h { (f32::from(last) * cell_h - 34.0).max(0.0) } else { below };
        let x = (f32::from(left) * cell_w).min((clip_w - 90.0).max(0.0));
        canvas_layer = canvas_layer.child(
            el().key("terminal-quote").absolute(x, y).child(render_quote_bar::<H>(p, ws_key, &tab.id, (cell_w, cell_h))),
        );
    }
    // The quote last taken: where its comment editor measures from, while its rows still hold;
    // once they don't, its source is gone for good (`quote-machine.ts:258-270`).
    let mark = tab.quote_source.get().and_then(|source| quote::origin(snap, source, cell_h));
    if mark.is_none() {
        tab.quote_source.set(None);
    }
    canvas_layer = canvas_layer.children(mark);

    let outer = row()
        .key(VIEWPORT_KEY)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .pl(PAD_X)
        .pr(PAD_X - SCROLLBAR_W)
        .pt(PAD_Y)
        .pb(PAD_Y)
        .cursor(if follows { Cursor::Pointer } else { Cursor::Text })
        .pointer_buttons([PointerButton::Middle]);
    let outer = attach_viewport_handlers::<H>(outer, input);

    let scrollbar = if snap.scrollbar.total > snap.scrollbar.len && snap.scrollbar.total > 0 {
        render_scrollbar::<H>(p, ws_key, &tab.id, &snap.scrollbar, body_h)
    } else {
        el().w(SCROLLBAR_W).h_full()
    };

    outer
        .children(render_marks(p, snap, cell_h))
        .child(canvas_layer)
        .child(spacer())
        .child(scrollbar)
}

#[allow(clippy::too_many_arguments)]
fn build_canvas_layer(
    host: &dyn TerminalSurfaceHost,
    p: &Pal,
    tab: &TerminalTab,
    snap: &TerminalSnapshot,
    style: TextStyle,
    rows: Vec<std::rc::Rc<rune::GridRow>>,
    hovered: &[(u16, u16, u16)],
    cols: u16,
    cell_w: f32,
    cell_h: f32,
    clip_w: f32,
    avail_h: f32,
    grid_h: f32,
    focused: bool,
    text_drag: impl FnOnce(El) -> El,
) -> El {
    // The screen is the Tab stop and the name, so a reader lands on its text and its caret (not
    // on a pane that repeats the whole screen); the viewport around it owns the keys.
    let rows_col = rune::grid(cols, cell_w, cell_h, rows)
        .key(SCREEN_KEY)
        .w(clip_w)
        .h(grid_h)
        .focusable(true)
        // The caret shows focus, never a ring (`packages/gui-plugins/src/terminal/terminal.tsx`: `outline-none`).
        .no_focus_ring()
        // A name that holds while a command runs: a shell retitles its window per command, and
        // each change of the focused element's name is said again. The tab shows the shell's.
        .label(opencode_i18n::t!(
            "rune.a11y.terminal",
            title = if tab.custom_title {
                tab.title.clone()
            } else {
                opencode_i18n::t!("plugin.terminal.title.numbered", number = tab.title_number.max(1))
            }
        ))
        .description_static(opencode_i18n::t!("rune.a11y.terminal.leave"));
    let rows_col = text_drag(rows_col);
    let rows_col = if snap.cursor.visible && snap.cursor.in_viewport {
        rows_col.grid_caret(snap.cursor.col, snap.cursor.row)
    } else {
        rows_col
    };
    let mut canvas_layer = el()
        .key(format!("terminal-canvas-{}", tab.id))
        .w(clip_w)
        .h(avail_h)
        .clip_soft(0.0)
        // Find's matches in view, under the text: the session's Find marks (`hit_bg`'s accent at 0.28), the
        // selected one in the accent at half strength so the terminal's text stays readable.
        .children(snap.find.iter().flat_map(|f| f.hits.iter()).enumerate().map(|(i, hit)| {
            let (x, y) = (f32::from(hit.start) * cell_w, f32::from(hit.row) * cell_h);
            let w = f32::from(hit.end.saturating_sub(hit.start) + 1) * cell_w;
            let fill = if hit.selected { p.accent.alpha(0.5) } else { p.accent.alpha(0.28) };
            el().key(format!("terminal-find-hit-{i}")).absolute(x, y).size(w, cell_h).fill(fill).inert()
        }))
        .child(rows_col);

    // Phase-locked cursor overlay (kept mounted with transparent fill during off-half).
    if snap.cursor.visible && snap.cursor.in_viewport {
        let lit = if focused && snap.cursor.blinking {
            host.terminal().cursor_lit(host.clock())
        } else {
            true
        };
        canvas_layer = canvas_layer.child(render_cursor(p, snap, cell_w, cell_h, focused, lit));
    }

    // The link under the pointer, underlined in its own colour (Windows Terminal and Ghostty do).
    canvas_layer = canvas_layer.children(hovered.iter().map(|&(r, a, b)| {
        let color = snap.screen_rows.get(usize::from(r)).and_then(|row| row.cells.get(usize::from(a))).map_or(p.text, cell_fg_color);
        let y = f32::from(r + 1) * cell_h - 2.0;
        el().key(format!("terminal-link-{r}")).absolute(f32::from(a) * cell_w, y).size(f32::from(b - a) * cell_w, 1.0).fill(color).inert()
    }));

    // The input method's caret at the cursor's cell; a password prompt keeps the input method off.
    if focused && !snap.cursor.password_input {
        let (col, row) = if snap.cursor.in_viewport { (snap.cursor.col, snap.cursor.row) } else { (0, snap.rows.saturating_sub(1)) };
        let (x, y) = (f32::from(col) * cell_w, f32::from(row) * cell_h);
        canvas_layer = canvas_layer
            .child(
                el().key("terminal-ime")
                    .absolute(x, y)
                    .size(cell_w, cell_h)
                    .a11y_hidden()
                    .ime_cursor()
                    .ime_option_as_alt(host.terminal_option_meta()),
            )
            .children(tab.preedit.as_ref().map(|pre| render_preedit(pre, snap, x, y, cell_h, style)));
    }

    canvas_layer
}

/// Finished commands (the shell's OSC 133 marks): a dot in the margin beside each one's prompt,
/// the success colour for exit 0 and the danger colour for any other, saying how it ended.
fn render_marks<'a>(p: &'a Pal, snap: &'a TerminalSnapshot, cell_h: f32) -> impl Iterator<Item = El> + 'a {
    let dot = 6.0;
    snap.marks.iter().map(move |&(row, mark)| {
        let took = took(mark.ms);
        let (color, said) = match mark.exit {
            Some(0) => (p.success, opencode_i18n::t!("rune.terminal.mark.ok", duration = took)),
            Some(code) => (p.danger, opencode_i18n::t!("rune.terminal.mark.failed", code = code.to_string(), duration = took)),
            None => (p.muted, opencode_i18n::t!("rune.terminal.mark.finished", duration = took)),
        };
        el().key(format!("terminal-mark-{row}"))
            .absolute((PAD_X - dot) / 2.0, PAD_Y + f32::from(row) * cell_h + (cell_h - dot) / 2.0)
            .size(dot, dot)
            .radius(dot / 2.0)
            .fill(color)
            .role("image")
            .label(said.clone())
            .help(said)
    })
}

/// A command's run time as the mark says it: `340 ms`, `2.4 s`, `3 min 5 s`.
fn took(ms: u32) -> String {
    match ms {
        0..1_000 => format!("{ms} ms"),
        1_000..60_000 => format!("{:.1} s", f64::from(ms) / 1_000.0),
        _ => format!("{} min {} s", ms / 60_000, ms / 1_000 % 60),
    }
}

fn render_cursor(
    p: &Pal,
    snap: &TerminalSnapshot,
    cell_w: f32,
    cell_h: f32,
    focused: bool,
    lit: bool,
) -> El {
    let x = f32::from(snap.cursor.col) * cell_w;
    let y = f32::from(snap.cursor.row) * cell_h;
    let cur_w = if snap.cursor.wide_tail { cell_w * 2.0 } else { cell_w };
    let color = snap.cursor_color.to_rune();

    let mut shape = Shape::new();
    if !focused || snap.cursor.shape == CursorShape::HollowBlock {
        if !focused || lit {
            let stroke = p.text.alpha(0.75);
            shape = shape.stroke_with(
                vec![
                    [0.5, 0.5],
                    [cur_w - 0.5, 0.5],
                    [cur_w - 0.5, cell_h - 0.5],
                    [0.5, cell_h - 0.5],
                ],
                true,
                1.0,
                LineCap::Butt,
                stroke,
            );
        }
    } else if lit {
        match snap.cursor.shape {
            CursorShape::Bar => {
                shape = shape.fill(
                    vec![[0.0, 0.0], [2.0, 0.0], [2.0, cell_h], [0.0, cell_h]],
                    color,
                );
            }
            CursorShape::Underline => {
                let uy = (cell_h - 2.0).max(0.0);
                shape = shape.fill(
                    vec![[0.0, uy], [cur_w, uy], [cur_w, cell_h], [0.0, cell_h]],
                    color,
                );
            }
            CursorShape::Block | CursorShape::HollowBlock => {
                shape = shape.fill(
                    vec![[0.0, 0.0], [cur_w, 0.0], [cur_w, cell_h], [0.0, cell_h]],
                    color.alpha(0.42),
                );
            }
        }
    }

    canvas(shape)
        .key("terminal-cursor")
        // The caret a reader follows is the screen's text selection; this draws it.
        .a11y_hidden()
        .absolute(x, y)
        .size(cur_w, cell_h)
        .instant_changes()
        .transition(rune::Transition::none())
        .inert()
}

/// The selection's last row and the first selected column on it.
fn selection_corner(snap: &TerminalSnapshot) -> Option<(u16, u16)> {
    snap.screen_rows
        .iter()
        .enumerate()
        .rev()
        .find_map(|(r, row)| row.bg_spans.iter().find(|s| s.selected).map(|s| (s.start_col, r as u16)))
}

/// The composition over the cells from the cursor on, on the terminal's background so the cells
/// under it don't show through, underlined as marked text is, with a caret where the input
/// method puts it.
fn render_preedit(
    pre: &input::Preedit,
    snap: &TerminalSnapshot,
    x: f32,
    y: f32,
    cell_h: f32,
    style: TextStyle,
) -> El {
    let fg = snap.fg.to_rune();
    let caret = pre.cursor.map(|(start, _)| rune::text::advance(pre.text.get(..start).unwrap_or(&pre.text), style));
    let mut span = Span::new(0, pre.text.len()).color(fg);
    span.underline = Some(fg);
    el().key("terminal-preedit")
        .absolute(x, y)
        .h(cell_h)
        .fill(snap.bg.to_rune())
        .inert()
        .child(rich(pre.text.clone(), vec![span]).font(style))
        .children(caret.map(|cx| el().key("terminal-preedit-caret").absolute(cx, 0.0).size(1.0, cell_h).fill(snap.cursor_color.to_rune())))
}
