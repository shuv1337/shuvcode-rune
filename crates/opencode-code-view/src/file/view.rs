//! Virtualized single-file viewer [`CodeViewer`], sharing [`CodeLook`] and syntax highlighting with [`DiffViewer`](crate::DiffViewer).

use std::ops::Range;
use std::rc::Rc;

use rune::{
    Anim, Color, Drag, El, GrowthSpec, Phase, ScrollSpec, Tx, col, memo, rich, row, text,
};
use rune_kit::ScrollBar;
use rune_kit::diff::Side;
use opencode_ui::palette::Pal;

use super::state::{CodeDocState, Content, FileLineSelection};
use crate::notes::{Handoff, LineNotes, LineRange};
use crate::{CodeLook, EDGE_FADE, LINE_H, TraceHooks, expand_tabs};

const INSTANT: Anim = Anim::INSTANT;

type Get<S> = Rc<dyn Fn(&S) -> &CodeDocState>;
type GetMut<S> = Rc<dyn Fn(&mut S) -> &mut CodeDocState>;

pub struct CodeViewer<S> {
    key: String,
    get: Get<S>,
    get_mut: GetMut<S>,
    notes: Option<Rc<dyn LineNotes<S>>>,
    trace: Option<TraceHooks>,
}

impl<S: 'static> CodeViewer<S> {
    pub fn new(
        key: impl Into<String>,
        get: impl Fn(&S) -> &CodeDocState + 'static,
        get_mut: impl Fn(&mut S) -> &mut CodeDocState + 'static,
    ) -> Self {
        Self::from_parts(key.into(), Rc::new(get), Rc::new(get_mut))
    }

    fn from_parts(key: String, get: Get<S>, get_mut: GetMut<S>) -> Self {
        Self {
            key,
            get,
            get_mut,
            notes: None,
            trace: None,
        }
    }

    /// Line comments on the file (`enableLineSelection`, `enableGutterUtility`, annotations).
    pub fn notes(mut self, notes: Rc<dyn LineNotes<S>>) -> Self {
        self.notes = Some(notes);
        self
    }

    /// Optional view/row build instrumentation hooks.
    pub fn trace(mut self, hooks: TraceHooks) -> Self {
        self.trace = Some(hooks);
        self
    }

    pub fn build(self, s: &S, p: &Pal) -> El {
        let st = (self.get)(s);
        let look = CodeLook::new(p);

        let root_key = self.key.clone();
        let scroll_key = format!("{root_key}-scroll");
        if let Some(t) = self.trace {
            (t.view)();
        }

        if let Some(view) = non_text_view(st, &look, p, &root_key) {
            return view;
        }

        let wrap = st.wrap();
        if !wrap && let Some(r) = rune::memo::layout(&scroll_key) {
            st.set_viewport(r);
        }
        st.begin_build();

        let count = st.line_count();
        let key_get = self.get.clone();
        let inner = Rc::new(Inner { look, pal: *p, v: self });
        // Pierre's file starts at its first line (`--diffs-gap-block: 0`, session-ui
        // `pierre/index.ts:329`), and Desktop's file tab leaves 160 px of the panel under the last
        // (`gui-plugins/src/file/view.tsx:480`, `pb-40`).
        let bar = crate::scroll_bar(p);
        let pad_r = crate::scroll_gutter(&bar, &scroll_key, &format!("{scroll_key}/rows"));
        let mut list = col()
            .key("rows")
            .w_full()
            .pb(160.0)
            .pr(pad_r)
            .growth(GrowthSpec::default())
            .selection_group(true)
            .role("list")
            .label_static(opencode_i18n::t!("rune.file.contents"));
        if !wrap {
            list = list.min_w(content_width(&inner.look, st).max(st.view_width()));
        }
        if let Some(notes) = inner.v.notes.clone() {
            let get = inner.v.get.clone();
            list = crate::notes::observe(list, root_key.clone(), notes, move |s, _| Some(get(s).path()));
        }
        let bands = count.div_ceil(crate::prepared::BAND_LINES as usize);
        let build_inner = inner.clone();
        let list = list
            .virtual_rows(
                bands,
                LINE_H * crate::prepared::BAND_LINES as f32,
                move |s: &S, b| key_get(s).band_key(b),
                move |s: &S, b| build_inner.band(s, b),
            )
            .virtual_rows_version(0u32);

        let root = wrap_scroll(root_key, scroll_key, wrap, p, bar, list);
        match inner.v.notes.clone() {
            Some(notes) => root.on_pointer_move_local(move |s: &mut S, _, tx: &mut Tx| notes.pointer_moved(s, tx)),
            None => root,
        }
    }
}

fn non_text_view(st: &CodeDocState, look: &CodeLook, p: &Pal, root_key: &str) -> Option<El> {
    match st.content() {
        Content::Loading => Some(
            col()
                .key(root_key.to_owned())
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .fill(p.bg_base)
                .center()
                .child(text(opencode_i18n::t!("rune.file.loading")).font(look.label).color(look.muted)),
        ),
        // A read that failed says so, and why, rather than "not found" (`files::FilePayload::Failed`).
        Content::Failed { what, why } => Some(
            col()
                .key(root_key.to_owned())
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .fill(p.bg_base)
                .center()
                .gap(4.0)
                .px(24.0)
                .child(text(what.clone()).key("file-read-failed").font(look.label).color(p.text))
                .child(text(why.clone()).font(look.label).color(look.muted).wrap().max_w(448.0).align(rune::Align::Center)),
        ),
        Content::Missing => {
            let name = opencode_kit::path::basename(st.path());
            Some(
                col()
                    .key(root_key.to_owned())
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .fill(p.bg_base)
                    .center()
                    .child(text(opencode_i18n::t!("plugin.file.tab.notFound", name = name)).font(look.label).color(look.muted)),
            )
        }
        Content::Binary { size } => Some(
            col()
                .key(root_key.to_owned())
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .fill(p.bg_base)
                .center()
                .child(
                    text(opencode_i18n::t!("rune.file.binary", size = *size))
                        .font(look.label)
                        .color(look.muted),
                ),
        ),
        Content::Text { .. } => None,
    }
}

fn content_width(look: &CodeLook, st: &CodeDocState) -> f32 {
    look.number_w(st.digits()) + (st.columns() + 2) as f32 * look.adv
}

fn wrap_scroll(root_key: String, scroll_key: String, wrap: bool, p: &Pal, bar: rune_kit::ScrollBarStyle, list: El) -> El {
    let scroll = col()
        .key(scroll_key)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: false });
    let scroll = if wrap { scroll } else { scroll.scroll_xy().scroll_edge_fade(EDGE_FADE) }.child(list).scroll_bar_styled(bar);
    // The rows on `background-stronger`, the panel's own background under them.
    col().key(root_key).w_full().grow(1.0).min_h(0.0).fill(p.bg_base).child(scroll)
}

fn row_matches_hash(st: &CodeDocState, n: u32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    st.line_matches(n).hash(&mut h);
    h.finish()
}

fn gutter_base(l: &CodeLook, st: &CodeDocState, n: u32, fill: Color, num_color: Color) -> El {
    // Pierre's number cell, as a diff's: 2ch in, at least 3ch, 1ch after
    // (`--opencode-diffs-number-inset`, session-ui `pierre/index.ts:270-272`).
    let mut gutter = row()
        .key("gutter")
        .w(l.number_w(st.digits()))
        .min_h(LINE_H)
        .self_stretch()
        .pr(l.adv)
        .justify_end()
        .items_start()
        .child(text(n.to_string()).font(l.mono).color(num_color));
    // Unwrapped, the numbers hold to the left edge over the code scrolling under them.
    if !st.wrap() {
        gutter = gutter.fill(fill).sticky_left(0.0);
    }
    gutter
}

fn code_col_base(l: &CodeLook, st: &CodeDocState, n: u32) -> El {
    let raw = st.line(n).unwrap_or("");
    let match_bgs: Vec<(Range<usize>, Color)> = st
        .line_matches(n)
        .iter()
        .map(|(r, active)| {
            (r.clone(), if *active { l.active_match_bg } else { l.match_bg })
        })
        .collect();
    let spans = l.spans(raw.len(), &st.highlights(n), &match_bgs, None);
    let (shown, spans) = expand_tabs(raw, spans);
    let mut code_txt = rich(shown, spans)
        .font(l.mono)
        .color(l.plain)
        .selectable(true)
        .selection_color(l.selection)
        .key(crate::notes::text_key(0, None, n));
    code_txt = if st.wrap() { code_txt.w_full().wrap_anywhere() } else { code_txt.single_line() };
    col().grow(1.0).min_w(0.0).px(l.adv).child(code_txt)
}

struct Inner<S> {
    look: CodeLook,
    pal: Pal,
    v: CodeViewer<S>,
}

impl<S: 'static> Inner<S> {
    /// Line `n` as the keyboard and a screen reader meet it: the view's one Tab stop is the
    /// cursor's line, Up and Down move it (`CodeDocState::line_key`), each line says its number
    /// and its code, and Return or C opens a comment on it, as the `+` does under the pointer.
    /// No ring in the viewer (`packages/session-ui/src/components/file.css`): the keyboard's line
    /// shows as the pointer's does, by its fill.
    fn line_stop(&self, st: &CodeDocState, n: u32, cursor: bool, el: El) -> El {
        let label = opencode_i18n::t!("rune.a11y.diff.line", line = n, code = st.line(n).unwrap_or("").trim());
        let get_mut = self.v.get_mut.clone();
        let notes = self.v.notes.clone();
        let path = st.path().to_owned();
        // File viewer takes focus on click for keyboard scroll/commenting and draws no ring,
        // without being a Tab stop (`packages/session-ui/src/components/file.tsx:904`, `opencode#65785`).
        let _ = cursor;
        el.focusable(true)
            .tab_index(-1)
            .focus_fill(self.look.hover)
            .no_focus_ring()
            .role("listitem")
            .label(label)
            .on_key(move |s: &mut S, ev: &rune::KeyEvent, tx: &mut Tx| {
                let plain = !(ev.mods.cmd || ev.mods.ctrl || ev.mods.alt);
                let comment = plain && (ev.key == rune::Key::Enter || matches!(&ev.key, rune::Key::Text(t) if t.eq_ignore_ascii_case("c")));
                if comment && let Some(notes) = &notes {
                    notes.plus(s, &path, None, n, tx);
                    return true;
                }
                let st = get_mut(s);
                // A line focused by a press is not the cursor's line yet, so it is not the viewer's
                // Tab stop: Tab would leave from the old cursor line's place (Shift+Tab from a
                // pressed line 2 landed on line 1). The cursor comes here first, then the key again.
                if ev.key == rune::Key::Tab && st.cursor != n {
                    st.cursor = n;
                    tx.send_key(ev.clone());
                    return true;
                }
                st.cursor = n;
                st.line_key(ev, tx)
            })
    }

    fn band(&self, s: &S, b: usize) -> El {
        let st = (self.v.get)(s);
        let first = b * crate::prepared::BAND_LINES as usize;
        let last = ((b + 1) * crate::prepared::BAND_LINES as usize).min(st.line_count());
        let mut out = col().w_full();
        for i in first..last {
            out = out.child(self.row(s, i));
        }
        out
    }

    fn row(&self, s: &S, i: usize) -> El {
        let st = (self.v.get)(s);
        let n = (i + 1) as u32;
        st.note_mounted_row(n);
        if let Some(t) = self.v.trace
            && (t.on)()
        {
            (t.row)(&format!("{}-scroll", self.v.key), n);
        }
        // A commented line shows as selected (`[data-comment-selected]`, session-ui
        // `pierre/index.ts:180-196`).
        // Rows build as they lay out, from the state then: the comments are read here.
        let notes_version = self.v.notes.as_ref().map_or(0, |notes| notes.version(s, st.path()));
        let commented = self.v.notes.as_ref().is_some_and(|notes| notes.commented(s, st.path(), Side::New, n));
        let selected = st.selection().is_some_and(|sel| sel.contains(n)) || commented;
        let quiet = self.v.notes.as_ref().is_some_and(|notes| notes.plus_quiet(s));
        // Find lights rows by what each holds, so a key rebuilds only rows whose matches moved.
        let matches = row_matches_hash(st, n);
        let lid = st.line_id(n);
        let hl_hash = st.highlight_hash(n);
        let row_memo_key = format!("cr{lid}");
        let line_key = format!("cl{n}");
        let height_key = line_key.clone();
        let get_mut = self.v.get_mut.clone();
        let cursor = st.cursor == n;
        memo(
            row_memo_key.as_str(),
            (
                self.look.version,
                st.path(),
                st.digits(),
                st.wrap(),
                n,
                hl_hash,
                selected,
                matches,
                notes_version,
                quiet,
                cursor,
            ),
            || {
                let line = self.line_row(st, n, hl_hash, matches, selected, quiet);
                let line = self.line_stop(st, n, cursor, line).key(line_key);
                let items = self.v.notes.as_ref().map_or_else(Vec::new, |notes| notes.annotations(s, &self.pal, st.path(), Side::New, n));
                let wrap_row = col().w_full().child(line);
                let wrap_row = if items.is_empty() {
                    wrap_row
                } else {
                    wrap_row.child(crate::notes::annotation_row(&self.pal, self.look.number_w(st.digits()), items))
                };
                crate::report_height(wrap_row, move |s: &mut S, h| get_mut(s).heights.record(&height_key, h))
            },
        )
    }

    fn line_row(
        &self,
        st: &CodeDocState,
        n: u32,
        hl_hash: u64,
        matches: u64,
        selected: bool,
        quiet: bool,
    ) -> El {
        let l = &self.look;
        let fill = if selected { l.selected } else { l.bg };
        let num_color = if selected { l.selected_number } else { l.faint };
        let get_mut = self.v.get_mut.clone();
        let notes = self.v.notes.clone();
        let gutter = gutter_base(l, st, n, fill, num_color).on_drag(move |s: &mut S, d: &mut Drag, tx: &mut Tx| {
            tx.instant();
            let st = get_mut(s);
            let before = st.selection();
            let unselect = st.unselecting();
            let path = st.path().to_owned();
            let ended = match d.phase {
                Phase::Down => {
                    st.press_line(n, tx.mods().shift);
                    false
                }
                Phase::Move => {
                    st.drag_to_offset(n, d.y);
                    false
                }
                Phase::Up if d.cancelled() => {
                    st.cancel_line();
                    false
                }
                Phase::Up => {
                    st.release_line();
                    true
                }
            };
            let after = st.selection().map(file_range);
            let Some(notes) = &notes else { return };
            if let (true, Some(line)) = (ended, unselect) {
                return notes.handoff(s, &path, Handoff::Unselected(file_range(line)), tx);
            }
            if before.map(file_range) != after {
                notes.handoff(s, &path, Handoff::Selected(after), tx);
            }
            if ended {
                notes.handoff(s, &path, Handoff::Ended(after), tx);
            }
        });

        let code_col = memo(
            "code",
            (l.version, st.wrap(), n, hl_hash, matches),
            || {
                let notes = self.v.notes.clone();
                let get_mut = self.v.get_mut.clone();
                code_col_base(l, st, n).on_background_press_tx(move |s: &mut S, tx| {
                    if !tx.touch_tap() {
                        return;
                    }
                    let st = get_mut(s);
                    let before = st.selection().map(file_range);
                    st.click_code(n);
                    let after = st.selection().map(file_range);
                    let path = st.path().to_owned();
                    if let Some(notes) = &notes {
                        if before != after {
                            notes.handoff(s, &path, Handoff::Selected(after), tx);
                        }
                        notes.handoff(s, &path, Handoff::Ended(after), tx);
                    }
                })
            },
        );

        let mut line = row()
            .w_full()
            .min_h(LINE_H)
            .items_start()
            .fill(fill)
            .role("listitem")
            .child(gutter)
            // The pointer's click, not a control: the keyboard selects and comments by the line's row.
            .child(code_col);
        if let Some(notes) = self.v.notes.as_ref().filter(|_| !quiet) {
            let notes = notes.clone();
            let path = st.path().to_owned();
            line = line.hover_group().child(crate::notes::plus_button(&self.pal, l.number_w(st.digits()), opencode_i18n::t!("ui.lineComment.submit"), move |s: &mut S, tx: &mut Tx| {
                notes.plus(s, &path, None, n, tx);
            }));
        }
        // A selected line keeps its fill under the pointer, as session-ui sets it outright; and
        // a line redrawn under a still pointer has lost its hover until it moves.
        if selected || quiet {
            return line;
        }
        line.hover(l.hover).hover_motion(INSTANT, INSTANT)
    }
}

/// A file view's selected lines as a comment's (a file's lines have no side).
fn file_range(sel: FileLineSelection) -> LineRange {
    LineRange { start: sel.start, end: sel.end, side: None, end_side: None }
}
