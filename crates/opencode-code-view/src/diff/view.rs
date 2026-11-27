//! Virtualized diff component [`DiffViewer`], shared by the Review panel and inline tool diffs.

use std::borrow::Cow;
use std::ops::Range;
use std::rc::Rc;

use rune::{Color, El, GrowthSpec, ScrollSpec, Tx, col, el, memo, row};
use rune_kit::ScrollBar;
use rune_kit::diff::{DiffMode, FileStatus, LineKind, LineRef, Side};
use opencode_ui::palette::Pal;
use opencode_ui::style::INSTANT;

use super::state::{DiffDocState, DiffRow};
use crate::notes::LineNotes;
use crate::{CodeLook, DiffTints, EDGE_FADE, LINE_H};

pub(super) type Get<S> = Rc<dyn Fn(&S) -> &DiffDocState>;
pub(super) type GetMut<S> = Rc<dyn Fn(&mut S) -> &mut DiffDocState>;
pub(super) type HeaderBuilder = Rc<dyn Fn(&Pal, &str, FileStatus, usize, usize) -> El>;

#[allow(clippy::type_complexity)]
pub struct DiffViewer<S> {
    pub(super) key: String,
    pub(super) get: Get<S>,
    pub(super) get_mut: GetMut<S>,
    pub(super) sticky_headers: bool,
    pub(super) whole: bool,
    pub(super) on_visible_file: Option<Rc<dyn Fn(&mut S, usize)>>,
    pub(super) on_fetch_text: Option<Rc<dyn Fn(&mut S, usize)>>,
    pub(super) header_actions: Option<Rc<dyn Fn(&S, usize) -> El>>,
    pub(super) file_header: Option<HeaderBuilder>,
    pub(super) notes: Option<Rc<dyn LineNotes<S>>>,
}

impl<S: 'static> DiffViewer<S> {
    pub fn new(
        key: impl Into<String>,
        get: impl Fn(&S) -> &DiffDocState + 'static,
        get_mut: impl Fn(&mut S) -> &mut DiffDocState + 'static,
    ) -> Self {
        Self::from_parts(key.into(), Rc::new(get), Rc::new(get_mut))
    }

    fn from_parts(key: String, get: Get<S>, get_mut: GetMut<S>) -> Self {
        Self {
            key,
            get,
            get_mut,
            sticky_headers: true,
            whole: false,
            on_visible_file: None,
            on_fetch_text: None,
            header_actions: None,
            file_header: None,
            notes: None,
        }
    }

    /// Line comments on the diff's files (`enableLineSelection`, `enableGutterUtility`,
    /// annotations).
    pub fn notes(mut self, notes: Rc<dyn LineNotes<S>>) -> Self {
        self.notes = Some(notes);
        self
    }

    pub fn sticky_headers(mut self, enabled: bool) -> Self {
        self.sticky_headers = enabled;
        self
    }

    /// Every row built at its own height, the diff as tall as they come and not scrolling
    /// itself: a wrapping diff shown whole inside another scroll.
    pub fn whole(mut self, whole: bool) -> Self {
        self.whole = whole;
        self
    }

    pub fn on_visible_file(mut self, f: impl Fn(&mut S, usize) + 'static) -> Self {
        self.on_visible_file = Some(Rc::new(f));
        self
    }

    /// Called with a file whose whole text a fold waits for (`FoldEffect::FetchText`); the app
    /// loads it into `DiffDocState::attach_file_text`, or `text_failed`.
    pub fn on_fetch_text(mut self, f: impl Fn(&mut S, usize) + 'static) -> Self {
        self.on_fetch_text = Some(Rc::new(f));
        self
    }

    pub fn header_actions(mut self, f: impl Fn(&S, usize) -> El + 'static) -> Self {
        self.header_actions = Some(Rc::new(f));
        self
    }

    /// Overrides the per-file header chrome (`default_file_header` is used when not set).
    pub fn file_header(mut self, f: impl Fn(&Pal, &str, FileStatus, usize, usize) -> El + 'static) -> Self {
        self.file_header = Some(Rc::new(f));
        self
    }

    pub fn build(self, s: &S, p: &Pal) -> El {
        let st = (self.get)(s);
        // Desktop draws a diff's rows on `background-stronger` (session-ui's `--diffs-bg`), the
        // panel's own background showing past the last of them.
        let look = CodeLook::new(p);
        let wrap = st.wraps();
        let root_key = self.key.clone();
        let scroll_path = format!("{root_key}/scroll-scroll-area/scroll");
        if !wrap && let Some(r) = rune::memo::layout(&scroll_path) {
            st.set_viewport(r);
        }
        let sticky_on = self.sticky_headers;
        let whole = self.whole && wrap;
        let inner = Rc::new(Inner { look, pal: *p, d: self });
        let count = st.rows.len();
        let list = col().key("rows").w_full().selection_group(true).role("list").label_static(opencode_i18n::t!("rune.diff.label"));
        let list = if let Some(notes) = inner.d.notes.clone() {
            let get = inner.d.get.clone();
            crate::notes::observe(list, root_key.clone(), notes, move |s, f| Some(get(s).files().get(f)?.path()))
        } else {
            list
        };
        if whole {
            let root = col().key(root_key).w_full().fill(p.bg_base).child(list.children((0..count).map(|i| inner.row(s, i))));
            return match inner.d.notes.clone() {
                Some(notes) => root.on_pointer_move_local(move |s: &mut S, _, tx: &mut Tx| notes.pointer_moved(s, tx)),
                None => root,
            };
        }
        let bar = crate::scroll_bar(p);
        let mut list = list.growth(GrowthSpec::default()).pr(crate::scroll_gutter(&bar, &scroll_path, &format!("{scroll_path}/rows")));
        if !wrap {
            list = list.min_w(content_width(&inner.look, st).max(st.view_width()));
        }
        let build_inner = inner.clone();
        let vis_mut = inner.d.get_mut.clone();
        let on_file = inner.d.on_visible_file.clone();
        let list = list
            .virtual_rows(count, LINE_H, row_keys(inner.d.get.clone()), move |s: &S, i| {
                build_inner.row(s, i)
            })
            .virtual_rows_version(st.keys_rev)
            .on_visible_rows(move |s: &mut S, rows| {
                let (reported, sticky_changed) = vis_mut(s).set_visible_rows(rows);
                if let (Some(f), Some(cb)) = (reported, &on_file) {
                    cb(s, f);
                    return true;
                }
                sticky_changed
            });
        let mut root = wrap_diff_scroll(root_key, wrap, p, bar, list);
        if let Some(notes) = inner.d.notes.clone() {
            root = root.on_pointer_move_local(move |s: &mut S, _, tx: &mut Tx| notes.pointer_moved(s, tx));
        }
        if sticky_on && let Some(f) = st.sticky() {
            root = root.child(
                inner
                    .file_header(s, st, f, true)
                    .key("sticky")
                    .pin(Some(0.0), Some(0.0), Some(0.0), None),
            );
        }
        root
    }
}

fn row_keys<S: 'static>(get: Get<S>) -> impl for<'a> Fn(&'a S, usize) -> Cow<'a, str> + 'static {
    move |s, i| Cow::Borrowed(get(s).keys[i].as_str())
}

#[derive(Clone, Copy)]
pub(super) struct CellData<'a> {
    pub(super) kind: LineKind,
    pub(super) old: Option<u32>,
    pub(super) new: Option<u32>,
    pub(super) text: &'a str,
    pub(super) changes: &'a [Range<usize>],
    pub(super) no_newline: bool,
}

impl CellData<'_> {
    pub(super) fn source(&self) -> (Side, u32) {
        match (self.kind, self.old, self.new) {
            (LineKind::Removed, Some(n), _) => (Side::Old, n),
            (_, _, Some(n)) => (Side::New, n),
            (_, Some(n), None) => (Side::Old, n),
            _ => (Side::New, 1),
        }
    }
}

pub(super) struct Inner<S> {
    pub(super) look: CodeLook,
    pub(super) pal: Pal,
    pub(super) d: DiffViewer<S>,
}

pub(super) fn tints(look: &CodeLook, kind: LineKind) -> Option<&DiffTints> {
    match kind {
        LineKind::Added => Some(&look.add),
        LineKind::Removed => Some(&look.del),
        LineKind::Context => None,
    }
}

/// A row's fill, its number cell's too: Pierre draws the cell in the line's colour.
fn fill(look: &CodeLook, kind: LineKind, selected: bool) -> Color {
    match (selected, tints(look, kind)) {
        (true, _) => look.selected,
        (false, Some(t)) => t.line,
        (false, None) => look.bg,
    }
}

/// Only an unchanged, unselected row lights under the pointer: Pierre mixes a changed one's
/// fill toward itself, and session-ui sets a selected one's outright (`index.ts:210-216`).
fn hover(look: &CodeLook, row: El, kind: LineKind, selected: bool, quiet: bool) -> El {
    if selected || quiet || kind != LineKind::Context {
        return row;
    }
    row.hover(look.hover).hover_motion(INSTANT, INSTANT)
}

/// One unwrapped line's width: its number cell, then its code 1ch in from each side.
fn content_width(look: &CodeLook, st: &DiffDocState) -> f32 {
    let digits = (0..st.files().len()).map(|f| st.digits(f)).max().unwrap_or(3);
    look.number_w(digits) + (st.columns() + 2) as f32 * look.adv
}

pub(super) fn view_w(st: &DiffDocState) -> Option<f32> {
    let w = st.view_width();
    (w > 0.0 && !st.wraps()).then_some(w)
}

/// Pierre's gutter: one number column, in unified mode as in each half of a split row.
pub(super) fn gutter_w(look: &CodeLook, st: &DiffDocState, f: usize) -> f32 {
    look.number_w(st.digits(f))
}

fn wrap_diff_scroll(root_key: String, wrap: bool, p: &Pal, bar: rune_kit::ScrollBarStyle, list: El) -> El {
    let scroll = col()
        .key("scroll")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: false });
    let scroll = if wrap { scroll } else { scroll.scroll_xy().scroll_edge_fade(EDGE_FADE) }.child(list).scroll_bar_styled(bar);
    col()
        .key(root_key)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(p.bg_base)
        .child(scroll)
}

fn cells(st: &DiffDocState, r: DiffRow) -> (Option<CellData<'_>>, Option<CellData<'_>>) {
    let of = |f: u32, h: u32, i: u32| {
        let l = &st.files()[f as usize].hunks[h as usize].lines[i as usize];
        CellData {
            kind: l.kind,
            old: l.old,
            new: l.new,
            text: &l.text,
            changes: &l.changes,
            no_newline: l.no_newline,
        }
    };
    match r {
        DiffRow::Ctx { f, old, new } => {
            let txt = st
                .side_line(f as usize, Side::New, new)
                .or_else(|| st.side_line(f as usize, Side::Old, old))
                .unwrap_or("");
            let c = CellData {
                kind: LineKind::Context,
                old: Some(old),
                new: Some(new),
                text: txt,
                changes: &[],
                no_newline: false,
            };
            (Some(c), None)
        }
        DiffRow::Line { f, h, i } => (Some(of(f, h, i)), None),
        DiffRow::Pair { f, h, del, add } => (Some(of(f, h, del)), Some(of(f, h, add))),
        _ => (None, None),
    }
}

fn row_content_hash(st: &DiffDocState, r: DiffRow) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    let f = r.file();
    (st.files()[f].path(), st.digits(f), st.mode(), st.wraps(), st.single_column(f)).hash(&mut h);
    let (a, b) = cells(st, r);
    for c in [a, b] {
        match c {
            None => 0u8.hash(&mut h),
            Some(c) => {
                let (src_side, n) = c.source();
                let text = if c.kind == LineKind::Context && st.mode() == DiffMode::Split {
                    st.side_line(f, src_side, n).unwrap_or(c.text)
                } else {
                    c.text
                };
                (
                    1u8,
                    c.kind,
                    c.old,
                    c.new,
                    text,
                    c.no_newline,
                    c.changes,
                    st.highlight_hash(f, src_side, n),
                    st.find_line(f, src_side, n),
                )
                    .hash(&mut h);
            }
        }
    }
    h.finish()
}

fn line_stop_meta(st: &DiffDocState, i: usize) -> (String, Option<(Side, u32)>) {
    let r = st.rows[i];
    let (a, b) = cells(st, r);
    let said = |c: &CellData<'_>| {
        let code = c.text.trim();
        match (c.kind, c.old, c.new) {
            (LineKind::Added, _, Some(n)) => opencode_i18n::t!("rune.a11y.diff.added", line = n, code = code),
            (LineKind::Removed, Some(n), _) => opencode_i18n::t!("rune.a11y.diff.removed", line = n, code = code),
            (_, _, n) => opencode_i18n::t!("rune.a11y.diff.line", line = n.or(c.old).unwrap_or(0), code = code),
        }
    };
    let label = [a.as_ref().map(said), b.as_ref().map(said)].into_iter().flatten().collect::<Vec<_>>().join("; ");
    // The line a comment goes on: the new side's, else the old one's.
    let at = b.or(a).and_then(|c| if c.kind == LineKind::Removed { c.old.map(|n| (Side::Old, n)) } else { c.new.map(|n| (Side::New, n)) });
    (label, at)
}

fn annotation_wrap(pal: &Pal, st: &DiffDocState, f: usize, gutter: f32, line: El, old: Vec<El>, new: Vec<El>) -> El {
    let annotation = match (st.mode(), st.single_column(f)) {
        (DiffMode::Split, None) => {
            let half = |k: &str, items: Vec<El>| {
                row()
                    .key(k.to_owned())
                    .w_frac(0.5)
                    .min_w(0.0)
                    .items_stretch()
                    .child(el().key("buffer").w(gutter))
                    .child(crate::notes::annotation_slots(items))
            };
            row()
                .key("annotation")
                .w_full()
                .items_stretch()
                .fill(crate::notes::comment_bg(pal))
                .child(half("old", old))
                .child(half("new", new))
        }
        _ => crate::notes::annotation_row(pal, gutter, old.into_iter().chain(new).collect()),
    };
    col().w_full().child(line.key("line")).child(annotation)
}

impl<S: 'static> Inner<S> {
    fn row(&self, s: &S, i: usize) -> El {
        let st = (self.d.get)(s);
        let r = st.rows[i];
        let key = st.keys[i].as_str();
        match r {
            DiffRow::File(f) if self.d.header_actions.is_none() => {
                let fu = f as usize;
                let file = &st.files()[fu];
                memo(
                    key,
                    (
                        self.look.version,
                        file.path(),
                        file.status,
                        file.additions(),
                        file.deletions(),
                        st.collapsed(fu),
                        view_w(st).map(f32::to_bits),
                    ),
                    || self.file_header(s, st, fu, false),
                )
            }
            DiffRow::File(f) => self.file_header(s, st, f as usize, false),
            DiffRow::Note(f) => {
                let fu = f as usize;
                let file = &st.files()[fu];
                memo(
                    key,
                    (
                        self.look.version,
                        file.path(),
                        st.digits(fu),
                        file.binary,
                        file.status,
                        &file.old_mode,
                        &file.new_mode,
                        view_w(st).map(f32::to_bits),
                    ),
                    || super::chrome::note_row(&self.look, st, fu),
                )
            }
            DiffRow::Gap { f, gap } => {
                let (fu, gu) = (f as usize, gap as usize);
                memo(
                    key,
                    (
                        self.look.version,
                        st.files()[fu].path(),
                        st.digits(fu),
                        st.hidden(fu, gu),
                        gu == 0,
                        gu == st.files()[fu].hunks.len(),
                        st.has_full_text(fu) || self.d.on_fetch_text.is_some(),
                        view_w(st).map(f32::to_bits),
                    ),
                    || self.gap_row(st, fu, gu),
                )
            }
            _ => {
                let notes = self.d.notes.as_ref().map(|n| {
                    let path = st.files()[r.file()].path();
                    let lines = [Side::Old, Side::New].map(|side| st.row_line(r, side).map(|n| (side, n)));
                    let commented = lines.map(|l| l.is_some_and(|(side, line)| n.commented(s, path, side, line)));
                    (n.version(s, path), commented)
                });
                let quiet = self.d.notes.as_ref().is_some_and(|n| n.plus_quiet(s));
                let version = (
                    self.look.version,
                    row_content_hash(st, r),
                    st.row_selected(i),
                    st.copy_side(),
                    st.selection().map(|sel| sel.start.side),
                    notes,
                    quiet,
                );
                let commented = notes.map_or([false; 2], |(_, c)| c);
                let get_mut = self.d.get_mut.clone();
                let cursor = st.cursor_row() == Some(i);
                let height_key = key.to_owned();
                memo(key, (version, cursor), || {
                    let line = self.line_row(st, i, commented, quiet);
                    let row = self.line_stop(st, i, cursor, self.with_annotations(s, st, i, line));
                    crate::report_height(row, move |s: &mut S, h| get_mut(s).heights.record(&height_key, h))
                })
            }
        }
    }

    /// Line row `i` as the keyboard and a screen reader meet it: one Tab stop for the whole diff
    /// (the cursor's line; the rest are reached by Up and Down, `DiffDocState::line_key`), named
    /// by what it is and says (added, removed, or a line of context, its number and its code), and
    /// Return or C opens a comment on it, as the `+` beside it does under the pointer. No ring in
    /// the viewer (`packages/session-ui/src/components/file.css`): the keyboard's line shows as
    /// the pointer's does, by its fill.
    fn line_stop(&self, st: &DiffDocState, i: usize, cursor: bool, el: El) -> El {
        let f = st.rows[i].file();
        let (label, at) = line_stop_meta(st, i);
        let get_mut = self.d.get_mut.clone();
        let notes = self.d.notes.clone();
        let path = st.files()[f].path().to_owned();
        // Diff viewer takes focus on click for keyboard scroll/commenting and draws no ring,
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
                if comment && let (Some(notes), Some((side, n))) = (&notes, at) {
                    notes.plus(s, &path, Some(side), n, tx);
                    return true;
                }
                let st = get_mut(s);
                // As in the file viewer: a line focused by a press becomes the cursor's line before
                // Tab leaves it, so Tab goes from this line, not from the old cursor line.
                if ev.key == rune::Key::Tab && st.cursor != Some(i as u32) {
                    st.cursor = Some(i as u32);
                    tx.send_key(ev.clone());
                    return true;
                }
                st.line_key(ev, tx)
            })
    }

    /// Row `i` with the annotations under its lines: one row across, or in a split diff a half
    /// under each side, the side without any empty (Pierre's annotation row and its buffer).
    fn with_annotations(&self, s: &S, st: &DiffDocState, i: usize, line: El) -> El {
        let Some(notes) = &self.d.notes else { return line };
        let r = st.rows[i];
        let f = r.file();
        let path = st.files()[f].path();
        let items = |side: Side| st.row_line(r, side).map_or_else(Vec::new, |n| notes.annotations(s, &self.pal, path, side, n));
        let (old, new) = (items(Side::Old), items(Side::New));
        if old.is_empty() && new.is_empty() {
            return line;
        }
        annotation_wrap(&self.pal, st, f, gutter_w(&self.look, st, f), line, old, new)
    }

    fn line_row(&self, st: &DiffDocState, i: usize, commented: [bool; 2], quiet: bool) -> El {
        let r = st.rows[i];
        let f = r.file();
        let (a, b) = cells(st, r);
        let Some(a) = a else { return col() };
        // A commented line shows as selected, both halves of its row in a split diff
        // (markCommentedDiffLines, session-ui pierre/commented-lines.ts).
        let selected = st.row_selected(i) || commented.contains(&true);
        match st.mode() {
            DiffMode::Unified => self.unified_line(st, i, f, a, selected, quiet),
            DiffMode::Split => {
                let (old, new) = match (b, a.kind) {
                    (Some(b), _) => (Some(a), Some(b)),
                    (None, LineKind::Removed) => (Some(a), None),
                    (None, LineKind::Added) => (None, Some(a)),
                    (None, LineKind::Context) => (Some(a), Some(a)),
                };
                self.split_line(st, i, f, old, new, selected, quiet)
            }
        }
    }

    /// A unified row: Pierre's one number column, a removed line's old number and any other's
    /// new one, then the code.
    fn unified_line(&self, st: &DiffDocState, i: usize, f: usize, c: CellData<'_>, selected: bool, quiet: bool) -> El {
        let digits = st.digits(f);
        let copy = match c.kind {
            LineKind::Context => true,
            LineKind::Added => st.copy_side() == Side::New,
            LineKind::Removed => st.copy_side() == Side::Old,
        };
        let (side, n) = if c.kind == LineKind::Removed { (Side::Old, c.old) } else { (Side::New, c.new) };
        let mut gutter = row()
            .key("gutter")
            .self_stretch()
            .items_stretch()
            .child(self.number_cell(st, i, f, n, side, c.kind, selected, digits).key("nn"));
        // Unwrapped, the numbers hold to the left edge over the code scrolling under them.
        if !st.wraps() {
            gutter = gutter.fill(fill(&self.look, c.kind, selected)).sticky_left(0.0);
        }
        let line = row()
            .w_full()
            .min_h(LINE_H)
            .items_start()
            .fill(fill(&self.look, c.kind, selected))
            .role("listitem")
            .child(gutter)
            .child(self.code_cell(st, f, &c, copy, side, n.map(|n| LineRef::new(side, n))).key("code"));
        let line = self.plus(st, f, n, side, quiet, line);
        hover(&self.look, line, c.kind, selected, quiet)
    }

    #[allow(clippy::too_many_arguments)]
    fn split_line<'a>(
        &self,
        st: &'a DiffDocState,
        i: usize,
        f: usize,
        old: Option<CellData<'a>>,
        new: Option<CellData<'a>>,
        selected: bool,
        quiet: bool,
    ) -> El {
        let digits = st.digits(f);
        let single = st.single_column(f);
        let half = |side: Side, c: Option<CellData<'a>>| -> El {
            let l = &self.look;
            let k = if side == Side::Old { "old" } else { "new" };
            let mut h = row()
                .key(k)
                .w_frac(if single.is_some() { 1.0 } else { 0.5 })
                .min_w(0.0)
                .min_h(LINE_H)
                .items_start()
                .self_stretch();
            // Session-ui's `--diffs-bg-context-gutter` where a side has no line
            // (`[data-content-buffer]`, `index.ts:175-178`).
            let Some(mut cell) = c else {
                return h.fill(l.empty_bg);
            };
            let kind = cell.kind;
            // Pierre selects rows: both sides of a split row light (`[data-selected-line]`).
            let n = if side == Side::Old { cell.old } else { cell.new };
            if kind == LineKind::Context {
                cell.text = st.side_line(f, side, n.unwrap_or(1)).unwrap_or(cell.text);
            }
            let copy = st.copy_side() == side;
            h = h
                .fill(fill(l, kind, selected))
                .child(self.number_cell(st, i, f, n, side, kind, selected, digits).key("n"))
                .child(self.code_cell(st, f, &cell, copy, side, n.map(|n| LineRef::new(side, n))).key("code"));
            let h = self.plus(st, f, n, side, quiet, h);
            hover(l, h, kind, selected, quiet)
        };
        // Two halves side by side, nothing between: session-ui takes away the 1 px of diff
        // background Pierre puts between them (`index.ts:167-173`). Desktop's review draws an
        // added or deleted file's one side as a single column.
        let line = row().w_full().min_h(LINE_H).items_stretch();
        match single {
            Some(Side::Old) => line.child(half(Side::Old, old)),
            Some(Side::New) => line.child(half(Side::New, new)),
            None => line.child(half(Side::Old, old)).child(half(Side::New, new)),
        }
    }
}
