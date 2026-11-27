//! Edit, Write and Patch, as a group shows them (`CurrentFileToolGroup` over the `patch`
//! renderer): consecutive file tools merge into one card of file rows (`FileAccordionGroup`),
//! a row per path they changed. A row (`accordion-trigger`) reads the file's icon, its muted
//! directory and name, then "Created", "Deleted" or its `+N -M`, and a grabber; pressed, the
//! change opens flush under it. Several changes to one path share a row: one net diff when each
//! starts where the last ended (`patchFileGroups`), else their diffs one after another. Files
//! start shut. With no file known yet, the first tool's title alone.

use rune::{El, col, icon, row, rule, text};
use rune_kit::diff::{FileDiff, parse};
use serde_json::Value;

use super::{Call, Ctx, Display, Press, TEXT_CONTEXT, Trigger, field, path, still, sub_key, trigger};
use crate::TimelineHost;
use crate::focus::FocusExt;
use crate::palette::Pal;
use crate::style::{HOVER_FADE, UI, UI_MED, readable};

/// A file row's `+N -M` (`DiffChanges`, standard, `diff-changes.css:30-37`): 14 px mono, regular (400), 150% line-height.
const COUNTS: rune::TextStyle = rune::TextStyle::new(14.0, 400.0).mono().lh(21.0);
/// A row's path: 13 px regular at 150% (`accordion-trigger`).
const PATH: rune::TextStyle = UI.lh(19.5);
/// `--radius-lg`: the card's outer corners.
const RADIUS: f32 = 8.0;

/// Whether `c` joins a run of file tools: an edit, write or patch that didn't fail.
pub fn is_file_tool(c: &Call) -> bool {
    *c.status != opencode_kit::model::ToolStatus::Error && matches!(c.name, "edit" | "multiedit" | "write" | "patch" | "apply_patch")
}

/// Whether file tool `c` draws a bordered file card (0 pt vertical overhang) rather than its
/// fallback 28 pt pending trigger (cheap check without computing a diff).
pub fn draws_card(c: &Call) -> bool {
    if let Some(files) = c.metadata["files"].as_array().filter(|f| !f.is_empty()) {
        return files.iter().any(|f| changed(c, f).is_some());
    }
    if path(c.input).is_none() {
        return false;
    }
    match c.name {
        "edit" | "multiedit" => c.input["oldString"].as_str().zip(c.input["newString"].as_str()).is_some_and(|(o, n)| o != n),
        "write" => field(c.input, "content").is_some(),
        _ => false,
    }
}

/// What a file's row says it did.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Add,
    Update,
    Delete,
}

/// One change a call made to a file: its patch, or the texts before and after.
enum Source<'a> {
    Patch(&'a str),
    Texts(&'a str, &'a str),
}

impl Source<'_> {
    fn diff(&self, path: &str) -> FileDiff {
        match self {
            Source::Patch(p) => parse(p).into_iter().next().unwrap_or_else(|| FileDiff::from_texts_with_context(path, "", "", TEXT_CONTEXT)),
            Source::Texts(old, new) => FileDiff::from_texts_with_context(path, old, new, TEXT_CONTEXT),
        }
    }

    /// The whole file before and after, when the patch holds all of it (Desktop's
    /// `completePatchContents`): one hunk from the first line, from a producer that writes
    /// full context (a git diff, or a header ending in a tab).
    fn contents(&self) -> Option<(String, String)> {
        let Source::Patch(p) = self else { return None };
        let full = p.starts_with("diff --git ") || p.lines().any(|l| l.starts_with("--- ") && l.ends_with('\t'));
        let fd = parse(p).into_iter().next().filter(|_| full)?;
        let [h] = fd.hunks.as_slice() else { return None };
        if h.old_start + u32::from(h.old_count == 0) > 1 || h.new_start + u32::from(h.new_count == 0) > 1 {
            return None;
        }
        let side = |keep: rune_kit::diff::LineKind| h.lines.iter().filter(|l| l.kind != keep).map(|l| format!("{}\n", l.text)).collect::<String>();
        Some((side(rune_kit::diff::LineKind::Added), side(rune_kit::diff::LineKind::Removed)))
    }
}

/// One change to a path, as one call reported it.
struct Entry<'a> {
    call: Call<'a>,
    path: &'a str,
    kind: Kind,
    additions: u64,
    deletions: u64,
    source: Source<'a>,
}

/// A row: a path and every change the run made to it.
struct Item<'a> {
    path: &'a str,
    entries: Vec<Entry<'a>>,
}

impl Item<'_> {
    fn kind(&self) -> Kind {
        match (self.entries.first().map(|e| e.kind), self.entries.last().map(|e| e.kind)) {
            (_, Some(Kind::Delete)) => Kind::Delete,
            (Some(Kind::Add), _) => Kind::Add,
            _ => Kind::Update,
        }
    }

    /// The net diff when each change starts where the one before ended.
    fn chained(&self) -> Option<FileDiff> {
        if self.entries.len() < 2 {
            return None;
        }
        let contents: Vec<(String, String)> = self.entries.iter().map(|e| e.source.contents()).collect::<Option<_>>()?;
        contents.windows(2).all(|w| w[0].1 == w[1].0).then(|| FileDiff::from_texts_with_context(self.path, &contents[0].0, &contents[contents.len() - 1].1, TEXT_CONTEXT))
    }

    fn counts(&self) -> (u64, u64) {
        match self.chained() {
            Some(fd) => (fd.additions() as u64, fd.deletions() as u64),
            None => self.entries.iter().fold((0u64, 0u64), |(a, d), e| (a.saturating_add(e.additions), d.saturating_add(e.deletions))),
        }
    }

    fn diffs(&self) -> Vec<FileDiff> {
        match self.chained() {
            Some(fd) => vec![fd],
            None => self.entries.iter().map(|e| e.source.diff(e.path)).collect(),
        }
    }
}

/// The changes call `c` made: its `metadata.files`, else the edit or write its input describes.
fn entries<'a>(c: &Call<'a>) -> Vec<Entry<'a>> {
    if let Some(files) = c.metadata["files"].as_array().filter(|f| !f.is_empty()) {
        // Entries that aren't changes show nothing, and still replace what the input says.
        return files.iter().filter_map(|f| changed(c, f)).collect();
    }
    let Some(path) = path(c.input) else { return Vec::new() };
    let entry = |source, additions, deletions| Entry { call: *c, path, kind: Kind::Update, additions, deletions, source };
    match c.name {
        "edit" | "multiedit" => {
            let (Some(old), Some(new)) = (c.input["oldString"].as_str(), c.input["newString"].as_str()) else { return Vec::new() };
            let fd = FileDiff::from_texts_with_context(path, old, new, TEXT_CONTEXT);
            let (a, d) = (fd.additions() as u64, fd.deletions() as u64);
            if a + d == 0 { Vec::new() } else { vec![entry(Source::Texts(old, new), a, d)] }
        }
        "write" => match field(c.input, "content") {
            Some(content) => vec![entry(Source::Texts("", content), (content.split('\n').count() - usize::from(content.ends_with('\n'))) as u64, 0)],
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// One of `metadata.files`, when it is a change (`changedFileDiff`).
fn changed<'a>(c: &Call<'a>, f: &'a Value) -> Option<Entry<'a>> {
    let (path, patch) = (f["file"].as_str()?, f["patch"].as_str()?);
    // A count is a whole number of lines: `1.0` counts (`rules::count`).
    let present = |k: &str| f.get(k).filter(|v| v.is_number()).map(crate::activity::rules::count);
    let (additions, deletions) = (present("additions")?, present("deletions")?);
    let kind = match f["status"].as_str()? {
        "added" => Kind::Add,
        "deleted" => Kind::Delete,
        "modified" => Kind::Update,
        _ => return None,
    };
    (additions > 0 || deletions > 0).then_some(Entry { call: *c, path, kind, additions, deletions, source: Source::Patch(patch) })
}

/// A run of file tools: one card of a row per path they changed, each shut until pressed or
/// open as Timeline detail's Edits say.
pub fn group<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, calls: &[Call]) -> El {
    group_with(s, p, cx, calls, s.timeline_detail().expanded(crate::detail::Category::Edit))
}

/// [`group`] with each file shut until pressed, whatever Timeline detail says, as Changed files
/// lists them (its `fileOpen` is only what was pressed).
pub fn shut_group<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, calls: &[Call]) -> El {
    group_with(s, p, cx, calls, false)
}

fn group_with<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, calls: &[Call], open: bool) -> El {
    let Some(first) = calls.first() else { return col() };
    let mut items: Vec<Item> = Vec::new();
    for e in calls.iter().flat_map(entries) {
        match items.iter_mut().find(|i| i.path == e.path) {
            Some(i) => i.entries.push(e),
            None => items.push(Item { path: e.path, entries: vec![e] }),
        }
    }
    if items.is_empty() {
        // Nothing changed yet: the first tool's title alone, its file count to come.
        let title = match first.name {
            "edit" | "multiedit" => opencode_i18n::t!("ui.messagePart.title.edit"),
            "write" => opencode_i18n::t!("ui.messagePart.title.write"),
            _ => opencode_i18n::t!("ui.tool.patch"),
        };
        let pending = calls.iter().find(|c| super::busy(c.status, cx.live)).unwrap_or(first);
        let d = Display::of(s, cx, pending, false);
        return still(trigger(s, p, cx, first, d, Trigger { title: title.into(), ..Default::default() }, Press::None, None));
    }
    // Each changed file is its own rounded card, separated by the small timeline gap
    // (`timeline-next.css:53-68`).
    let mut card = col().key("files").w_full().gap(crate::timeline::rhythm::S);
    let mut keyed: Vec<&str> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let owner = item.entries[0].call;
        // The first row a call shows carries its key, so opening the call from the host keeps it in view.
        let row_key = if keyed.contains(&owner.id) { format!("file-{}-{i}", first.id) } else { format!("tool-{}", owner.id) };
        keyed.push(owner.id);
        card = card.child(file_item(s, p, cx, first.id, item, row_key, open));
    }
    card
}

/// One file's card, and under it the change while open, and the file's errors.
#[allow(clippy::too_many_arguments)]
fn file_item<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, group: &str, item: &Item, row_key: String, default_open: bool) -> El {
    let shown = super::session_relative(s, cx.session, item.path);
    let k = sub_key(group, format!("file:{shown}"));
    // Until pressed, `default_open` decides: Timeline detail's Edits, or shut in Changed files.
    let open = crate::disclosure::is_open(s, &k, default_open);
    let owner = item.entries[0].call;
    let errors = diagnostics(p, owner.metadata, path(owner.input).unwrap_or(item.path));
    // The card's bottom corners round whichever section ends it: the errors, the change or the row.
    let end = errors.is_none();
    let top = RADIUS;
    let bottom = if end && !open { RADIUS } else { 0.0 };
    let (dir, name) = match shown.rfind(['/', '\\']) {
        Some(at) => (&shown[..=at], &shown[at + 1..]),
        None => ("", shown),
    };
    // A path reads left to right in any window, beside its icon: `src/util/` keeps its slash
    // last and `format.ts` stays after it.
    let mut words = row().key("name").dir(rune::Direction::Ltr).shrink().min_w(0.0).center_y();
    if !dir.is_empty() {
        words = words.child(text(dir.to_owned()).key("dir").font(PATH).color(p.muted).truncate_head().shrink().min_w(0.0));
    }
    words = words.child(text(name.to_owned()).key("file").font(PATH).color(p.text).truncate_fade().shrink().min_w(0.0));
    let change = change(p, item);
    // File diffs open and close without animating their parent group's fold glide
    // (`packages/session-ui/src/timeline-next/tools/patch.tsx` `FileAccordionItem`,
    // `packages/ui/src/data-display/accordion/accordion.css`).
    let toggle = || crate::disclosure::set_instant::<S>(k.clone(), open, row_key.clone());
    // `apply-patch-file-info` (`patch.tsx:77-105`, `timeline-next.css:89-105`): the change counts
    // follow the file's name, and the grabber sits 4 pt closer to the right edge so its arrows
    // balance the file icon on the left.
    let info = row()
        .key("info")
        .h_full()
        .gap(8.0)
        .center_y()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .child(icon(rune_theme_opencode::file_icons::file(name), 16.0).key("icon").color(p.text))
        .child(words)
        .children(change);
    let marks = row()
        .key("marks")
        .h_full()
        .center_y()
        .child(icon(&rune_theme_opencode::icons::CHEVRON_GRABBER_VERTICAL, 16.0).key("grabber").mr(-4.0).color(p.text));
    // The row eases its hover in and out over 0.15 s (`accordion.css`: `transition:
    // background-color 0.15s ease`).
    let trigger = row()
        .key(row_key.clone())
        .role("button")
        .label(shown.to_owned())
        .expanded(open)
        .w_full()
        .h(32.0)
        .px(12.0)
        .gap(20.0)
        .center_y()
        .focus_default(p)
        .fill(p.bg_base)
        // Half the usual hover overlay, so the header only hints that it toggles (`timeline-next.css:116-117`).
        .hover(p.hover.alpha(p.hover.a * 0.5))
        .border(1.0, p.border)
        .border_layout()
        .corners([top, top, bottom, bottom])
        .hover_group_focus()
        .hover_motion(HOVER_FADE, HOVER_FADE)
        .on_press_tx(toggle())
        .child(info)
        .child(marks);
    // The row sticks over its change as the timeline scrolls, under an open group's label
    // (`sticky-accordion-header`, `message-part.css:1330-1332`), until the change's end takes it
    // away. Square and on `bg-base` behind the row (`[data-scope="apply-patch"]
    // [data-slot="accordion-header"]`), so the lines scrolling under it don't show at its
    // rounded corners.
    let header = row().key("head").w_full().fill(p.bg_base).sticky_top(cx.sticky).child(trigger);
    // A key holds no `/`, which separates the parts of a path.
    let mut out = col().key(format!("item-{}", shown.replace(['/', '\\'], ":"))).w_full().child(header);
    if open {
        let corner = if end { RADIUS } else { 0.0 };
        let diffs = item.diffs();
        let views = col().key("views").w_full().children(diffs.into_iter().map(|fd| super::diff::view::<S>(p, fd)));
        // Flush under the row, a hairline over the change (`edit-content`); the row's bottom
        // border is the content's top, which has none of its own (`border-top: 0`). No reveal
        // transition: official mounts the diff directly (`patch.tsx:157-167`, `accordion.css`)
        // without re-animating sibling rows or the enclosing group.
        out = out.child(
            col()
                .key("content")
                .w_full()
                .border_sides([0.0, 1.0, 1.0, 1.0], p.border)
                .border_layout()
                .fill(p.layer1)
                .corners([0.0, 0.0, corner, corner])
                .clip_corners([0.0, 0.0, corner, corner])
                .transition(rune::Transition::none())
                .child(rule(0.5, p.border_muted).key("hairline"))
                .child(readable(p, views)),
        );
    }
    if let Some(errors) = errors {
        // An intentional Rune extra: Desktop lists a file's errors under a lone edit or write but
        // never in a group's file card. Here they close the file's part of the card, 1 pt up so
        // their divider stands in for the bottom border of the row or the change above.
        let inner = (RADIUS - 1.0).max(0.0);
        out = out.child(
            col()
                .key("errors")
                .w_full()
                .mt(-1.0)
                .border_sides([0.0, 1.0, 1.0, 1.0], p.border)
                .border_layout()
                .fill(p.bg_base)
                .corners([0.0, 0.0, RADIUS, RADIUS])
                .clip_corners([0.0, 0.0, RADIUS, RADIUS])
                .child(errors.corners([0.0, 0.0, inner, inner])),
        );
    }
    out
}

/// "Created", "Deleted" (13 px medium) or `+N` / `-M`, showing only the side that changed
/// (`[data-component="diff-changes"] [data-empty] { display: none }`, `timeline-next.css:99-101`).
fn change(p: &Pal, item: &Item) -> Option<El> {
    let word = |w: &'static str, c| Some(text(w).key("change").font(UI_MED.lh(19.5)).color(c));
    match item.kind() {
        Kind::Add => word(opencode_i18n::t!("ui.patch.action.created"), p.diff.add_icon),
        Kind::Delete => word(opencode_i18n::t!("ui.patch.action.deleted"), p.diff.del_icon),
        Kind::Update => {
            let (a, d) = item.counts();
            (a > 0 || d > 0).then(|| {
                row()
                    .key("change")
                    .gap(8.0)
                    .center_y()
                    .children((a > 0).then(|| text(format!("+{a}")).dir(rune::Direction::Ltr).font(COUNTS).color(p.diff.add_text)))
                    .children((d > 0).then(|| text(format!("-{d}")).dir(rune::Direction::Ltr).font(COUNTS).color(p.diff.del_text)))
            })
        }
    }
}
/// A script's change without a diff to show (`ScriptChanges`' `file-counts`, past the size cap,
/// a binary file or counts alone): the file's icon, its muted directory and name, then "Created",
/// "Deleted" or its `+N -M`, each count only where it isn't zero (`markEmpty`). 13 px on a line
/// 6 above and below, with no card around it, so such rows sit flush.
pub fn counts_row<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, key: String, file: &Value) -> El {
    let path = file["file"].as_str().unwrap_or("");
    let shown = super::session_relative(s, cx.session, path);
    let (dir, name) = match shown.rfind(['/', '\\']) {
        Some(at) => (&shown[..=at], &shown[at + 1..]),
        None => ("", shown),
    };
    let mut words = row().key("name").dir(rune::Direction::Ltr).grow(1.0).shrink().min_w(0.0).center_y();
    if !dir.is_empty() {
        words = words.child(text(dir.to_owned()).key("dir").font(PATH).color(p.muted).truncate_head().shrink().min_w(0.0));
    }
    words = words.child(text(name.to_owned()).key("file").font(PATH).color(p.text).truncate_fade().shrink().min_w(0.0));
    // "Created" and "Deleted" read as the counts do (`diff-changes-additions`,
    // `diff-changes-deletions`): 14 pt mono in the diff's text colours.
    let word = |w: &'static str, c| text(w).key("change").font(COUNTS).color(c);
    let (a, d) = (crate::activity::rules::count(&file["additions"]), crate::activity::rules::count(&file["deletions"]));
    let change = match file["status"].as_str() {
        Some("added") => Some(word(opencode_i18n::t!("ui.patch.action.created"), p.diff.add_text)),
        Some("deleted") => Some(word(opencode_i18n::t!("ui.patch.action.deleted"), p.diff.del_text)),
        _ if a > 0 || d > 0 => Some(
            row().key("change").dir(rune::Direction::Ltr).gap(8.0).center_y()
                .children((a > 0).then(|| text(format!("+{a}")).dir(rune::Direction::Ltr).font(COUNTS).color(p.diff.add_text)))
                .children((d > 0).then(|| text(format!("-{d}")).dir(rune::Direction::Ltr).font(COUNTS).color(p.diff.del_text))),
        ),
        _ => None,
    };
    row()
        .key(key)
        .w_full()
        .py(6.0)
        .gap(8.0)
        .center_y()
        .role("group")
        .label(shown.to_owned())
        .child(icon(rune_theme_opencode::file_icons::file(name), 16.0).key("icon").color(p.text).a11y_hidden())
        .child(words)
        .children(change)
}

/// A file's errors after the change (severity 1, the first three), as Desktop's
/// `DiagnosticsDisplay` lists them (`[data-component="diagnostics"]`): 13/19.5 mono in
/// `state-fg-danger` on `state-bg-danger`, 8 by 12 in, under a 0.5 pt `state-border-danger`
/// divider.
fn diagnostics(p: &Pal, metadata: &Value, file: &str) -> Option<El> {
    let list: Vec<&Value> = metadata["diagnostics"][file].as_array()?.iter().filter(|d| d["severity"] == 1).take(3).collect();
    if list.is_empty() {
        return None;
    }
    let font = rune::TextStyle::new(13.0, 400.0).mono().lh(19.5);
    let label = rune::TextStyle::new(13.0, 530.0).mono().lh(19.5).tracking(-0.5);
    let mut box_ = col()
        .key(format!("diagnostics-{}", file.replace(['/', '\\'], ":")))
        .w_full()
        .gap(4.0)
        .px(12.0)
        .py(8.0)
        .fill(p.danger_bg)
        .border_sides([0.5, 0.0, 0.0, 0.0], p.danger_border)
        .border_layout();
    for d in list {
        let start = &d["range"]["start"];
        let at = format!("[{}:{}]", start["line"].as_u64().unwrap_or(0) + 1, start["character"].as_u64().unwrap_or(0) + 1);
        box_ = box_.child(
            row().w_full().gap(6.0)
                .child(text("ERROR").font(label).color(p.danger))
                .child(text(at).font(font).color(p.danger))
                .child(readable(p, text(d["message"].as_str().unwrap_or("").to_owned()).font(font).color(p.danger).wrap_anywhere().max_lines(3)).shrink().min_w(0.0)),
        );
    }
    Some(box_)
}
