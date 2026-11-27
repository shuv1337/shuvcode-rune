//! Markdown files read as prose in the file viewer: drawn with the replies' renderer and look,
//! a toolbar toggle to the source, ⌘F over the rendered text, and which files wrap by default.

use std::ops::Range;

use opencode_code_view::CodeDocState;
use rune::{El, Icon, Tx, col, memo};
use rune_kit::Markdown;
use rune_kit::markdown::Case;
use serde_json::Value;
use opencode_ui::palette::{Elevate as _, Pal, kit};
use opencode_ui::style::{markdown_mark, markdown_style};

use super::links::slug;
use crate::{FilesHost, SessionFiles};

/// The rendered column's width: a comfortable line for reading.
pub const READ_W: f32 = 720.0;

/// Bigger Markdown files open as source. Rune lays a Markdown document out whole, with no
/// virtualized rows: in a release build a 9,700-line, 835 KB file took 137 ms to open and
/// 1.5 ms a scrolled frame, so the cap sits just past what was measured.
pub use opencode_code_view::data::RENDER_LIMIT;

/// Storage namespace for persisted per-kind line wrap preferences.
pub const WRAP_STORAGE_NS: &str = "plugin.file";
/// Storage key for persisted per-kind line wrap preferences.
pub const WRAP_STORAGE_KEY: &str = "rune.wrap";

pub static READ_ICON: Icon = Icon {
    name: "files-read-rendered",
    view: 16.0,
    body: r#"<path d="M3 3.5H13M3 6.5H10M3 9.5H13M3 12.5H9" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" fill="none"/>"#,
};

/// How a previewable file is displayed (`packages/gui-plugins/src/file/artifact-view-machine.ts`
/// `ArtifactMode`, plus `Auto` for Rune's [`RENDER_LIMIT`] size cap before the user picks a mode).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PreviewMode {
    /// Default before any user toggle: preview for SVGs and files up to [`RENDER_LIMIT`], source above it.
    #[default]
    Auto,
    /// User explicitly chose the rendered preview.
    Preview,
    /// User explicitly chose the source view (or followed a line link into it).
    Source,
}

/// Events driving [`PreviewMode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewModeEvent {
    /// Switch to the opposite of the currently resolved display (`currently_preview`).
    Toggle { currently_preview: bool },
    /// Force source view (for example when following a link to specific line numbers).
    ShowSource,
}

impl PreviewMode {
    pub fn shows_preview(self, default_preview: bool) -> bool {
        match self {
            Self::Auto => default_preview,
            Self::Preview => true,
            Self::Source => false,
        }
    }

    pub fn is_forced_source(self) -> bool {
        matches!(self, Self::Source)
    }

    /// Transitions to the next mode; returns whether the resolved mode is now preview.
    pub fn step(&mut self, event: PreviewModeEvent) -> bool {
        *self = match event {
            PreviewModeEvent::Toggle { currently_preview: true } | PreviewModeEvent::ShowSource => Self::Source,
            PreviewModeEvent::Toggle { currently_preview: false } => Self::Preview,
        };
        matches!(*self, Self::Preview)
    }
}

/// A Markdown file's view: rendered or source, and its matches in the rendered text.
#[derive(Default)]
pub struct MdView {
    /// Kit's pick, rendered (`Preview`) or source (`Source`); `Auto` is rendered up to [`RENDER_LIMIT`].
    pub mode: PreviewMode,
    applied: String,
    pub matches: Vec<Vec<Range<usize>>>,
    pub active: usize,
}

fn ext(path: &str) -> Option<String> {
    let name = opencode_kit::path::basename(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => Some(ext.to_ascii_lowercase()),
        _ => None,
    }
}

pub fn is_markdown(path: &str) -> bool {
    ext(path).is_some_and(|e| matches!(e.as_str(), "md" | "markdown" | "mdx"))
}

/// Prose rather than code: Markdown, text, reStructuredText, AsciiDoc, and the extensionless
/// files a project keeps beside its code (`LICENSE`, `README`).
pub fn is_prose(path: &str) -> bool {
    const NAMES: &[&str] = &[
        "LICENSE",
        "LICENCE",
        "README",
        "COPYING",
        "NOTICE",
        "AUTHORS",
        "CHANGELOG",
        "CONTRIBUTORS",
        "PATENTS",
    ];
    let name = opencode_kit::path::basename(path);
    match ext(path) {
        Some(e) => matches!(e.as_str(), "md" | "markdown" | "mdx" | "txt" | "rst" | "adoc"),
        None => {
            let upper = name.to_ascii_uppercase();
            NAMES
                .iter()
                .any(|n| upper.strip_prefix(n).is_some_and(|rest| rest.is_empty() || rest.starts_with(['-', '_'])))
        }
    }
}

/// Wrap per kind of file: every file wraps, as Desktop's file tab does (Pierre's `overflow:
/// "wrap"`, session-ui `pierre/index.ts:307`), until flipped (kept across launches).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WrapPrefs {
    pub prose: Option<bool>,
    pub code: Option<bool>,
}

impl WrapPrefs {
    pub fn wraps(&self, path: &str) -> bool {
        if is_prose(path) { self.prose.unwrap_or(true) } else { self.code.unwrap_or(true) }
    }

    /// A document for `path`, wrapped as its kind is.
    pub fn doc(&self, path: &str, text: &str) -> CodeDocState {
        let mut d = CodeDocState::new(path, text);
        d.set_wrap(self.wraps(path));
        d
    }

    /// Parses wrap choices from a stored JSON object.
    pub fn from_value(value: Option<Value>) -> Self {
        let v = value.unwrap_or_default();
        Self { prose: v["prose"].as_bool(), code: v["code"].as_bool() }
    }

    /// The File plugin's wrap choices, under Rune's own key of its store (`plugin.file` `rune.wrap`).
    pub fn load(host: &dyn FilesHost) -> Self {
        Self::from_value(host.read_json(WRAP_STORAGE_NS, WRAP_STORAGE_KEY))
    }

    fn save(&self, host: &dyn FilesHost) {
        host.write_json(
            WRAP_STORAGE_NS,
            WRAP_STORAGE_KEY,
            &serde_json::json!({ "prose": self.prose, "code": self.code }),
        );
    }
}

/// Whether `path` shows rendered now: Markdown as a page, an SVG as its picture (Desktop's
/// preview, which an SVG opens in).
pub fn shows_rendered(ws: &SessionFiles, path: &str) -> bool {
    let Some(d) = ws.docs.get(path) else { return false };
    let opencode_code_view::Content::Text { size } = *d.content() else { return false };
    let svg = super::image::is_svg(path);
    if !(is_markdown(path)
        || svg
        || ws.html.contains_key(path)
        || ws.tables.contains_key(path)
        || ws.diagram.current(d).is_some())
    {
        return false;
    }
    let default_preview = svg || size <= RENDER_LIMIT;
    ws.md
        .get(path)
        .map_or(default_preview, |m| m.mode.shows_preview(default_preview))
}

pub fn scroll_key(sid: &str, path: &str) -> String {
    format!("file-md-{sid}-{path}-scroll")
}

fn heading_key(slug: &str) -> String {
    format!("mdh-{slug}")
}

/// Each top-level ATX heading (`## Usage`): where its line starts, and its slug.
fn headings(src: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    let mut at = 0;
    for line in src.split_inclusive('\n') {
        let t = line.trim_end_matches(['\n', '\r']);
        let lead = t.trim_start();
        if lead.starts_with("```") || lead.starts_with("~~~") {
            let mark = &lead[..3];
            match fence {
                Some(m) if m == mark => fence = None,
                None => fence = Some(mark),
                _ => {}
            }
        } else if fence.is_none() && t.starts_with('#') {
            let n = t.bytes().take_while(|b| *b == b'#').count();
            if n <= 6 && t[n..].starts_with(' ') {
                out.push((at, slug(t[n..].trim().trim_end_matches('#'))));
            }
        }
        at += line.len();
    }
    out
}

/// The rendered file: the replies' Markdown in a reading column that scrolls, its links
/// resolving against the file's folder. `page` lays it out as Desktop's artifact view does: on
/// `layer-01`, a raised `document-surface` sheet 20 in (12 in a view 600 wide or narrower) with its 0.5 pt
/// `border-base` ring and 8 pt corners, the text 64 from its top and 12.5% in, between 24 and
/// 134 (32 and 24 in a narrow view), at most 768 wide.
pub fn rendered<H: FilesHost + 'static>(
    p: &Pal,
    sid: &str,
    path: &str,
    doc: &CodeDocState,
    view: Option<&MdView>,
    paths: Option<&super::code_paths::CodePaths>,
    page: bool,
) -> El {
    let find = view.filter(|_| doc.find_open());
    let active = find.map(|v| v.active);
    let find_ver = find.map(|v| (v.applied.clone(), v.matches.len(), v.active));
    let (sid_link, path_link) = (sid.to_owned(), path.to_owned());
    let base = std::rc::Rc::new(opencode_kit::path::parent(path).to_owned());
    // Inline code naming a file there is a link to it, in `text-code-path` (`markdown.css`).
    let found: std::rc::Rc<std::collections::HashSet<String>> = std::rc::Rc::new(
        paths
            .map(|c| c.found().map(str::to_owned).collect())
            .unwrap_or_default(),
    );
    let body = memo(
        format!("md-body-{sid}-{path}"),
        (doc.keys_rev, doc.rev, p.theme, p.generation, p.dark, opencode_i18n::locale().id(), find_ver, paths.map(|c| c.rev)),
        || {
            let src = doc.text();
            let marks: Vec<(&[Range<usize>], rune_kit::Highlight)> = find
                .map(|v| {
                    v.matches
                        .iter()
                        .enumerate()
                        .map(|(i, r)| (r.as_slice(), markdown_mark(p, i, Some(i) == active)))
                        .collect()
                })
                .unwrap_or_default();
            Markdown::new(src)
                // Outside the timeline, so not its selection colour (`quote-highlight.css`).
                .style(markdown_style(p, p.text, READ_W).selection(p.selection))
                .highlights(marks)
                .block_keys(headings(src).into_iter().map(|(at, s)| (at, heading_key(&s))))
                .selectable(true)
                // Official Markdown sets `overflow-wrap: break-word` (`packages/session-ui/src/components/markdown.css:54`).
                .wrap_anywhere(true)
                .on_link_tx(move |s: &mut H, url: &str, tx: &mut Tx| {
                    crate::links::open_link(s, &sid_link, Some(&path_link), url, tx);
                })
                .link_help({
                    let base = base.clone();
                    move |url| crate::link_tip::tip(url, base.as_str())
                })
                .code_link(p.v2.text_code_path, {
                    let found = found.clone();
                    move |code| found.contains(code).then(|| code.to_owned())
                })
                .build(kit(p))
        },
    );
    let key = scroll_key(sid, path);
    if page {
        let [_, _, w, h] = memo::layout(&key).unwrap_or([0.0, 0.0, 800.0, 0.0]);
        let narrow = w <= 600.0;
        let (gutter, pad_y, pad_x) = if narrow {
            (12.0, 32.0, 24.0)
        } else {
            (20.0, 64.0, (w - 40.0) * 0.125)
        };
        let foot = if narrow { 12.0 } else { 20.0 };
        let sheet = col()
            .key("artifact-markdown-page")
            .w_full()
            // `min-height: 100%`: the sheet runs to the view's foot however short the text.
            .min_h((h - 1.0 - foot).max(0.0))
            .items_center()
            .px(pad_x.clamp(24.0, 134.0))
            .py(pad_y)
            .radius(8.0)
            .fill(p.bg_base)
            .elevate(p.raised)
            .border(0.5, p.border)
            .child(body.w_full().max_w(768.0));
        // `[data-document-surface-scrollport]` (`document-surface.css:23-26`): `scrollbar-gutter: stable`
        // keeps the 8 pt gutter beside the sheet whether or not the document overflows.
        return opencode_ui::scroller::scroller(key).stable_gutter().build::<H>(
            p,
            col().w_full().grow(1.0).min_h(0.0).fill(p.layer1),
            col().w_full().px(gutter).pt(1.0).pb(foot).child(sheet),
        );
    }
    opencode_ui::scroller::scroller(key).build::<H>(
        p,
        col().w_full().grow(1.0).min_h(0.0).fill(p.bg_base),
        col()
            .w_full()
            .items_center()
            .px(28.0)
            .pt(20.0)
            .pb(48.0)
            .child(body.w_full().max_w(READ_W)),
    )
}

/// Switches `path` between rendered and source.
pub fn toggle_rendered(host: &mut dyn FilesHost, sid: &str, path: &str, tx: &mut Tx) {
    tx.instant();
    let Some(ws) = host.files_mut().sessions.get_mut(sid) else { return };
    let currently_preview = shows_rendered(ws, path);
    if let Some(doc) = ws.docs.get_mut(path) {
        doc.set_find_open(false);
    }
    if let Some(table) = ws.tables.get(path) {
        ws.table_find.forget(table);
    }
    if let Some(page) = ws.html.get(path) {
        ws.html_find.forget(page);
    }
    if let Some(drawing) = ws.shown_diagram(path) {
        ws.diagram_find.forget(&drawing);
    }
    let next = ws
        .md
        .entry(path.to_owned())
        .or_default()
        .mode
        .step(PreviewModeEvent::Toggle { currently_preview });
    if next && let Some(d) = ws.docs.get_mut(path) {
        d.set_selection(None);
    }
}

/// Turns wrap on or off for `path`, and for every file of its kind from now on.
pub fn set_file_wrap(host: &mut dyn FilesHost, sid: &str, path: &str, wrap: bool) {
    let prose = is_prose(path);
    let prefs = {
        let pane = host.files_mut();
        if prose {
            pane.wrap.prose = Some(wrap);
        } else {
            pane.wrap.code = Some(wrap);
        }
        for ws in pane.sessions.values_mut() {
            for (p, d) in ws.docs.iter_mut() {
                if is_prose(p) == prose {
                    d.set_wrap(wrap);
                }
            }
        }
        if let Some(d) = pane.sessions.get_mut(sid).and_then(|w| w.docs.get_mut(path)) {
            d.set_wrap(wrap);
        }
        pane.wrap
    };
    if host.persists() {
        prefs.save(host);
    }
}

/// Finds the find bar's query in the rendered text of `path` when it changed; whether it did.
pub fn sync_rendered_find(host: &mut dyn FilesHost, sid: &str, path: &str, tx: &mut Tx) -> bool {
    let eligible = host.files().sessions.get(sid).is_some_and(|ws| {
        shows_rendered(ws, path) && ws.docs.get(path).is_some_and(|d| d.find_open())
    });
    if !eligible {
        return false;
    }
    let Some(ws) = host.files_mut().sessions.get_mut(sid) else { return false };
    let Some(doc) = ws.docs.get(path).filter(|d| d.find_open()) else { return false };
    if let Some(page) = ws.html.get(path) {
        if !ws.html_find.sync(page, &doc.find_input.text) {
            return false;
        }
        ws.html_find.reveal(sid, path, tx);
        return true;
    }
    if let Some(drawing) = ws.shown_diagram(path) {
        if !ws.diagram_find.sync(&drawing, &doc.find_input.text) {
            return false;
        }
        ws.diagram_find.reveal(sid, path, tx);
        return true;
    }
    if let Some(table) = ws.tables.get(path) {
        if !ws.table_find.sync(table, &doc.find_input.text) {
            return false;
        }
        ws.table_find.reveal(sid, path, tx);
        return true;
    }
    let q = doc.find_input.text.clone();
    let v = ws.md.entry(path.to_owned()).or_default();
    if v.applied == q {
        return false;
    }
    let fold = rune_kit::search::Fold::for_language(opencode_i18n::locale().tag());
    v.matches = rune_kit::markdown::find(doc.text(), &q, Case::Folded(fold));
    v.applied = q;
    v.active = 0;
    // A new query lands its first match at once, however far down it is
    // (`gui-plugins/src/file/find.tsx`: `search(query, 0)`, `scrollIntoView({ block: "center" })`).
    if !v.matches.is_empty() {
        tx.scroll_center_in_instant(scroll_key(sid, path), "find-active");
    }
    true
}

/// The find bar's next (or previous) match in `path`, in whichever view shows.
pub fn file_find_step(host: &mut dyn FilesHost, sid: &str, path: &str, forward: bool, tx: &mut Tx) {
    let Some(ws) = host.files_mut().sessions.get_mut(sid) else { return };
    if !shows_rendered(ws, path) {
        if let Some(d) = ws.docs.get_mut(path) {
            d.sync_find_query();
            d.step_match(&format!("file-code-{sid}-{path}-scroll"), forward, tx);
        }
        return;
    }
    sync_rendered_find(host, sid, path, tx);
    let Some(ws) = host.files_mut().sessions.get_mut(sid) else { return };
    if ws.shown_html(path).is_some() {
        ws.html_find.step(forward);
        ws.html_find.reveal(sid, path, tx);
        return;
    }
    if ws.shown_diagram(path).is_some() {
        ws.diagram_find.step(forward);
        ws.diagram_find.reveal(sid, path, tx);
        return;
    }
    if ws.shown_table(path).is_some() {
        ws.table_find.step(forward);
        ws.table_find.reveal(sid, path, tx);
        return;
    }
    let Some(v) = ws.md.get_mut(path) else { return };
    let n = v.matches.len();
    if n == 0 {
        return;
    }
    v.active = if forward { (v.active + 1) % n } else { (v.active + n - 1) % n };
    tx.scroll_center_in_instant(scroll_key(sid, path), "find-active");
}

/// Scrolls `path` to its heading `anchor`: in the rendered view, or to its line in the source.
pub fn scroll_to_heading(host: &mut dyn FilesHost, sid: &str, path: &str, anchor: &str, tx: &mut Tx) {
    let want = slug(anchor);
    let info = host.files().sessions.get(sid).and_then(|ws| {
        let doc = ws.docs.get(path)?;
        let at = headings(doc.text()).into_iter().find(|(_, s)| *s == want).map(|(at, _)| at)?;
        Some((shows_rendered(ws, path), at))
    });
    let Some((rendered, at)) = info else { return };
    if rendered {
        tx.scroll_key_to(heading_key(&want), 12.0);
    } else if let Some(d) = host
        .files_mut()
        .sessions
        .get_mut(sid)
        .and_then(|ws| ws.docs.get_mut(path))
    {
        let line = d.text()[..at].matches('\n').count() as u32 + 1;
        d.scroll_to_line(&format!("file-code-{sid}-{path}-scroll"), line, false, tx);
    }
}
