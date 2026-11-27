//! Settings › About › Third-party notices (`settings/about/notices/dialog.tsx`): an x-large
//! scrollable dialog listing OpenCode (Rune)'s bundled third-party notices, clickable upstream
//! repository links, verbatim notices, and collapsible full license texts parsed from the
//! repository's `THIRD_PARTY_NOTICES.md`.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use opencode_i18n::t;
use opencode_ui::focus::FocusExt;
use opencode_ui::palette::{Elevate as _, Pal};
use opencode_ui::scroller::scroller;
use opencode_ui::widgets::{self, DialogSize, Fit, dialog_window};
use rune::{Cursor, Cx, El, Link, LinkPaint, Span, TextStyle, Tx, col, el, row, rule, text};

use crate::ABOUT_NOTICES_OVERLAY_ID;
use crate::host::SettingsHost;

const TITLE: TextStyle = TextStyle::new(15.0, 530.0).lh(20.0).tracking(-0.13);
const DESC: TextStyle = TextStyle::new(13.0, 440.0).lh(20.0).tracking(-0.04);
const HEADING: TextStyle = TextStyle::new(13.0, 540.0).lh(20.0).tracking(-0.04);
const BODY: TextStyle = TextStyle::new(13.0, 440.0).lh(20.0).tracking(-0.04);
const COMPACT: TextStyle = TextStyle::new(13.0, 440.0).lh(16.0).tracking(-0.04);
const META: TextStyle = TextStyle::new(12.0, 440.0).lh(16.0);
const PRE: TextStyle = TextStyle::new(11.0, 440.0).lh(16.0).mono();

const RAW_NOTICES: &str = include_str!("../../../../../THIRD_PARTY_NOTICES.md");

/// Whether the Third-party notices dialog is open and which license disclosures are expanded.
#[derive(Default)]
pub struct NoticesState {
    pub open: bool,
    pub expanded: BTreeSet<usize>,
}

/// Opens the Third-party notices dialog and registers it on the overlay stack.
pub fn open(host: &mut dyn SettingsHost, tx: &mut Tx) {
    tx.instant();
    let was_open = host.settings_pages().about_notices.open;
    host.settings_pages_mut().about_notices.open = true;
    if !was_open {
        host.on_overlay_opened(ABOUT_NOTICES_OVERLAY_ID, None);
    }
}

/// Closes the Third-party notices dialog if open.
pub fn close(host: &mut dyn SettingsHost) {
    let was_open = host.settings_pages().about_notices.open;
    let st = &mut host.settings_pages_mut().about_notices;
    st.open = false;
    st.expanded.clear();
    if was_open {
        host.on_overlay_closed(ABOUT_NOTICES_OVERLAY_ID);
    }
}

fn close_tx(host: &mut dyn SettingsHost, tx: &mut Tx) {
    tx.instant();
    close(host);
}

fn toggle_disclosure(host: &mut dyn SettingsHost, id: usize, tx: &mut Tx) {
    tx.instant();
    let set = &mut host.settings_pages_mut().about_notices.expanded;
    if !set.remove(&id) {
        set.insert(id);
    }
}

struct Section {
    title: String,
    blocks: Vec<Block>,
}

enum Block {
    Subheading(String),
    Paragraph(String),
    Entry {
        name: String,
        url: Option<String>,
        version: Option<String>,
        license: Option<String>,
        notes: Vec<String>,
    },
    Pre(String),
    Disclosure {
        id: usize,
        title: String,
        text: String,
    },
}

fn sections() -> &'static [Section] {
    static PARSED: OnceLock<Vec<Section>> = OnceLock::new();
    PARSED.get_or_init(|| parse_notices(RAW_NOTICES))
}

/// The Third-party notices modal dialog, while open.
pub fn dialog<H: SettingsHost + 'static>(s: &H, p: &Pal, cx: &Cx<H>) -> Vec<El> {
    let st = &s.settings_pages().about_notices;
    if !st.open {
        return Vec::new();
    }
    let mut list = col().key("settings-notices").w_full().px(20.0).pb(20.0).gap(20.0);
    for (idx, sec) in sections().iter().enumerate() {
        if idx > 0 {
            list = list.child(rule(0.5, p.border));
        }
        list = list.child(render_section::<H>(p, idx, sec, &st.expanded));
    }
    let scroll = scroller("settings-about-notices").build::<H>(
        p,
        col().key("settings-notices-scroll").w_full().grow(1.0).shrink().min_h(0.0),
        list,
    );
    let title = t!("settings.about.notices.title");
    let header = row()
        .key("about-notices-header")
        .w_full()
        .padding(16.0)
        .gap(8.0)
        .items_start()
        .child(
            col()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(4.0)
                .child(text(title).font(TITLE).color(p.text).wrap().w_full())
                .child(text(t!("settings.about.notices.description")).font(DESC).color(p.muted).wrap().w_full()),
        )
        .child(close_button::<H>(p));
    let inner = col().w_full().grow(1.0).shrink().min_h(0.0).clip_rounded(12.0).child(header).child(scroll);
    let card = col()
        .key("about-notices-card")
        .role("dialog")
        .label_static(title)
        .radius(12.0)
        .fill(p.bg_base)
        .elevate(p.overlay)
        .modal_tx(true, |s: &mut H, tx: &mut Tx| close_tx(s, tx))
        .child(inner);
    widgets::sized_modal("about-notices", p, DialogSize::XLarge, Fit::Fixed, dialog_window(cx, s.zoom()), card)
}

fn close_button<H: SettingsHost + 'static>(p: &Pal) -> El {
    el().key("about-notices-close")
        .role("button")
        .label_static(t!("common.close"))
        .size(20.0, 20.0)
        .mt(-2.0)
        .mr(-2.0)
        .radius(4.0)
        .center()
        .focus_default(p)
        .hover(p.hover)
        .press(p.pressed)
        .on_press_tx(|s: &mut H, tx: &mut Tx| close_tx(s, tx))
        .child(rune::icon(&widgets::DIALOG_CLOSE, 16.0).color(p.icon_muted))
}

fn render_section<H: SettingsHost + 'static>(p: &Pal, idx: usize, sec: &Section, expanded: &BTreeSet<usize>) -> El {
    let mut out = col().key(format!("notice-sec-{idx}")).w_full().gap(8.0);
    out = out.child(text(sec.title.clone()).font(HEADING).color(p.text).wrap().w_full());
    for (bi, block) in sec.blocks.iter().enumerate() {
        let el_block = match block {
            Block::Subheading(h) => text(h.clone()).font(HEADING).color(p.text).wrap().w_full().mt(6.0),
            Block::Paragraph(body) => text(body.clone()).font(BODY).color(p.muted).wrap().w_full(),
            Block::Entry { name, url, version, license, notes } => render_entry::<H>(p, idx, bi, name, url.as_deref(), version.as_deref(), license.as_deref(), notes),
            Block::Pre(body) => text(body.clone()).font(PRE).color(p.muted).wrap_anywhere().w_full(),
            Block::Disclosure { id, title, text: license_text } => render_disclosure::<H>(p, *id, title, license_text, expanded.contains(id)),
        };
        out = out.child(el_block);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn render_entry<H: SettingsHost + 'static>(
    p: &Pal,
    sec_idx: usize,
    bi: usize,
    name: &str,
    url: Option<&str>,
    version: Option<&str>,
    license: Option<&str>,
    notes: &[String],
) -> El {
    let lead = match (url, version) {
        (Some(href), Some(ver)) => {
            let full = format!("{name} {ver}");
            let end = name.len();
            text(full)
                .font(COMPACT)
                .color(p.faint)
                .spans(vec![Span::new(0, end).color(p.text).underline(p.text)])
                .link(Link::new(0..end, href).role("link").label(name).hover(LinkPaint::new().color(p.text).underline(p.text)))
                // An `ExternalLink` outside `.settings-about-content`: `a { cursor: default }` (`base.css:84`).
                .cursor(rune::Cursor::Default)
                .on_link(|s: &mut H, u: &str, tx: &mut Tx| crate::open_external(s, u, tx))
                .wrap()
        }
        (Some(href), None) => text(name.to_owned())
            .font(HEADING)
            .color(p.text)
            .spans(vec![Span::new(0, name.len()).underline(p.text)])
            .link(Link::new(0..name.len(), href).role("link").label(name).hover(LinkPaint::new().color(p.text).underline(p.text)))
            .cursor(rune::Cursor::Default)
            .on_link(|s: &mut H, u: &str, tx: &mut Tx| crate::open_external(s, u, tx))
            .wrap(),
        (None, Some(ver)) => text(format!("{name} {ver}")).font(COMPACT).color(p.text).wrap(),
        (None, None) => text(name.to_owned()).font(HEADING).color(p.text).wrap(),
    };
    let heading = row()
        .w_full()
        .gap(12.0)
        .items_start()
        .child(col().grow(1.0).shrink().min_w(0.0).child(lead))
        .children(license.map(|lic| text(lic.to_owned()).font(COMPACT).color(p.faint)));
    let mut entry = col().key(format!("notice-entry-{sec_idx}-{bi}")).w_full().gap(2.0).child(heading);
    if !notes.is_empty() {
        entry = entry.child(text(notes.join("\n")).font(META).color(p.faint).wrap_anywhere().w_full());
    }
    entry
}

fn render_disclosure<H: SettingsHost + 'static>(p: &Pal, id: usize, title: &str, body: &str, open: bool) -> El {
    let glyph = if open { "▾ " } else { "▸ " };
    let label = format!("{glyph}{title}");
    let summary = row()
        .key(format!("notice-disclosure-{id}"))
        .role("button")
        .label(title.to_owned())
        .expanded(open)
        .cursor(Cursor::Pointer)
        .focus_default(p)
        .hover_group_focus()
        .radius(4.0)
        .on_press_tx(move |s: &mut H, tx: &mut Tx| toggle_disclosure(s, id, tx))
        .child(text(label).font(BODY).color(p.faint).group_color(p.text));
    let mut out = col().w_full().gap(8.0).child(summary);
    if open {
        out = out.child(
            col()
                .key(format!("notice-license-{id}"))
                .w_full()
                .padding(12.0)
                .radius(8.0)
                .fill(p.layer2)
                .child(text(body.to_owned()).font(PRE).color(p.muted).wrap_anywhere().w_full()),
        );
    }
    out
}

fn parse_notices(raw: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<Section> = None;
    let mut pending_heading: Option<String> = None;
    let mut disclosure_id = 0usize;
    let lines: Vec<&str> = raw.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed == "---" || trimmed.starts_with("# ") {
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            if let Some(sec) = current.take() {
                sections.push(sec);
            }
            pending_heading = None;
            current = Some(Section { title: strip_leading_number(rest).to_owned(), blocks: Vec::new() });
            i += 1;
            continue;
        }
        let Some(sec) = current.as_mut() else {
            i += 1;
            continue;
        };
        if let Some(rest) = trimmed.strip_prefix("### ") {
            let heading = clean_inline(rest);
            // If the next non-empty line is a fenced code block, use this heading as its disclosure title.
            let next_non_empty = lines[i + 1..].iter().find(|l| !l.trim().is_empty()).map(|l| l.trim());
            if next_non_empty.is_some_and(|l| l.starts_with("```")) {
                pending_heading = Some(heading);
            } else {
                sec.blocks.push(Block::Subheading(heading));
            }
            i += 1;
            continue;
        }
        if trimmed.starts_with("```") {
            let mut body_lines = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                body_lines.push(lines[i]);
                i += 1;
            }
            if i < lines.len() {
                i += 1;
            }
            let dedented = dedent(&body_lines);
            if body_lines.len() <= 12 && pending_heading.is_none() {
                sec.blocks.push(Block::Pre(dedented));
            } else {
                let title = pending_heading.take().unwrap_or_else(|| t!("settings.about.notices.license").to_owned());
                sec.blocks.push(Block::Disclosure { id: disclosure_id, title, text: dedented });
                disclosure_id += 1;
            }
            continue;
        }
        // Markdown table (`| Crate | Version | Source |`)
        if trimmed.starts_with('|') {
            let cells: Vec<&str> = trimmed.trim_matches('|').split('|').map(str::trim).collect();
            let is_sep = cells.iter().all(|c| c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' '));
            let is_header = cells.first().is_some_and(|c| c.eq_ignore_ascii_case("crate"));
            if !is_sep && !is_header && cells.len() >= 3 {
                let name = clean_inline(cells[0]);
                let version = clean_inline(cells[1]);
                let url = extract_url(cells[2]);
                sec.blocks.push(Block::Entry {
                    name,
                    url,
                    version: Some(version),
                    license: Some("MPL-2.0".to_owned()),
                    notes: Vec::new(),
                });
            }
            i += 1;
            continue;
        }
        // Top-level bullet or numbered item (`- **Name** ...` or `1. **Name** ...`)
        if let Some(item_rest) = strip_top_list_marker(line) {
            let (name, url, license, inline_note) = parse_entry_head(item_rest);
            let mut notes = Vec::new();
            if let Some(n) = inline_note {
                notes.push(n);
            }
            let mut nested_blocks: Vec<Block> = Vec::new();
            i += 1;
            while i < lines.len() {
                let sub = lines[i];
                let st = sub.trim();
                if st.is_empty() {
                    break;
                }
                if !sub.starts_with(' ') && !sub.starts_with('\t') {
                    break;
                }
                if st.starts_with("```") {
                    let mut body_lines = Vec::new();
                    i += 1;
                    while i < lines.len() && !lines[i].trim().starts_with("```") {
                        body_lines.push(lines[i]);
                        i += 1;
                    }
                    if i < lines.len() {
                        i += 1;
                    }
                    nested_blocks.push(Block::Pre(dedent(&body_lines)));
                    continue;
                }
                if let Some(bullet) = st.strip_prefix("- ") {
                    let cleaned = clean_inline(bullet);
                    if !cleaned.is_empty() && !cleaned.ends_with("NOTICE:") {
                        notes.push(cleaned);
                    }
                } else {
                    notes.push(clean_inline(st));
                }
                i += 1;
            }
            sec.blocks.push(Block::Entry { name, url, version: None, license, notes });
            sec.blocks.extend(nested_blocks);
            continue;
        }

        // Plain paragraph
        sec.blocks.push(Block::Paragraph(clean_inline(trimmed)));
        i += 1;
    }

    if let Some(sec) = current {
        sections.push(sec);
    }
    sections
}

fn strip_leading_number(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut k = 0;
    while k < bytes.len() && bytes[k].is_ascii_digit() {
        k += 1;
    }
    if k > 0 && s[k..].starts_with(". ") { &s[k + 2..] } else { s }
}

fn strip_top_list_marker(line: &str) -> Option<&str> {
    if line.starts_with(' ') || line.starts_with('\t') {
        return None;
    }
    if let Some(rest) = line.strip_prefix("- ") {
        return Some(rest);
    }
    let bytes = line.as_bytes();
    let mut k = 0;
    while k < bytes.len() && bytes[k].is_ascii_digit() {
        k += 1;
    }
    if k > 0 && line[k..].starts_with(". ") { Some(&line[k + 2..]) } else { None }
}

fn parse_entry_head(s: &str) -> (String, Option<String>, Option<String>, Option<String>) {
    let url = extract_url(s);
    if let Some(after_open) = s.strip_prefix("**")
        && let Some(close) = after_open.find("**")
    {
        let name = clean_inline(&after_open[..close]).trim_end_matches(':').to_owned();
        let rest = after_open[close + 2..].trim();
        let (before_dash, after_dash) = split_license_dash(rest);
        let license = after_dash.map(clean_inline).filter(|l| !l.is_empty());
        // Keep extra non-URL detail if meaningful (e.g. copyright inline after `:`)
        let detail = before_dash.trim().trim_start_matches(':').trim();
        let inline_note = if detail.is_empty() || (detail.starts_with('(') && detail.ends_with(')') && url.is_some() && !detail.contains(',')) {
            None
        } else {
            let cleaned = clean_inline(detail);
            (!cleaned.is_empty()).then_some(cleaned)
        };
        return (name, url, license, inline_note);
    }
    (clean_inline(s), url, None, None)
}

fn split_license_dash(s: &str) -> (&str, Option<&str>) {
    // Split on the last ` — ` or ` - ` outside parentheses (or a leading `— `/`- ` when an entry
    // has no parenthesized URL, such as OpenMono), iterating on UTF-8 char boundaries.
    if let Some(rest) = s.strip_prefix("— ").or_else(|| s.strip_prefix("- ")) {
        return ("", Some(rest.trim()));
    }
    let mut depth = 0usize;
    let mut last_split: Option<(usize, usize)> = None;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                if s[i..].starts_with(" — ") {
                    last_split = Some((i, " — ".len()));
                } else if s[i..].starts_with(" - ") {
                    last_split = Some((i, " - ".len()));
                }
            }
            _ => {}
        }
    }
    match last_split {
        Some((at, len)) => (&s[..at], Some(s[at + len..].trim())),
        None => (s, None),
    }
}

fn extract_url(s: &str) -> Option<String> {
    let start = s.find("https://").or_else(|| s.find("http://"))?;
    let tail = &s[start..];
    let end = tail.find(|c: char| matches!(c, '`' | ')' | ',' | ' ' | '"' | '\'' | '<' | '>')).unwrap_or(tail.len());
    let u = tail[..end].trim_end_matches('.');
    (!u.is_empty()).then(|| u.to_owned())
}

fn clean_inline(s: &str) -> String {
    // Strip markdown `[label](#anchor)` -> `label`, `**bold**` -> `bold`, `` `code` `` -> `code`.
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'['
            && let Some(mid) = s[i + 1..].find("](")
            && let Some(end) = s[i + 1 + mid + 2..].find(')')
        {
            out.push_str(&s[i + 1..i + 1 + mid]);
            i += 1 + mid + 2 + end + 1;
            continue;
        }
        if s[i..].starts_with("**") {
            i += 2;
            continue;
        }
        if bytes[i] == b'`' {
            i += 1;
            continue;
        }
        let Some(ch) = s[i..].chars().next() else { break };
        out.push(ch);
        i += ch.len_utf8();
    }
    out.trim().to_owned()
}

fn dedent(lines: &[&str]) -> String {
    let min_indent = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start_matches(' ').len()).min().unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            let strip = l.bytes().take(min_indent).take_while(|&b| b == b' ').count();
            if strip == min_indent { &l[strip..] } else { l.trim_start() }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}
