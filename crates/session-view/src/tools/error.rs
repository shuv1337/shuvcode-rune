//! A failed call: Desktop's `ToolErrorCard`. Shut, a 24 pt trigger of the warning mark, the
//! tool's name, what it was working on, the error's gist in red and an arrow; open, the whole
//! error and a Copy error button, with a subtitle the trigger had to cut moved above it.

use rune::{El, Tx, col, el, row, text};

use super::{Call, directory, field, file_name, files_count, path};
use crate::TimelineHost;
use crate::focus::FocusExt;
use crate::palette::Pal;
use crate::style::{INSTANT, TOOL_CODE, UI, UI_MED, arrow, readable};

/// The card of failed call `c` in session `session`, among a turn's rows: its 24 pt trigger
/// (`tool-error-card.css`) in a 28 pt line, the rest of the 12 pt between Desktop's tool rows
/// over the 8 these rows keep.
pub fn card<S: TimelineHost>(s: &S, p: &Pal, session: &str, c: &Call) -> El {
    card_with(s, p, session, c, super::default_open(s, c), 28.0)
}

/// The card of failed call `c`, open until pressed when `default_open`, its trigger's line
/// `line_h` tall: 24 under a shell command, as Desktop's card sits right under its console.
pub fn card_with<S: TimelineHost>(s: &S, p: &Pal, session: &str, c: &Call, default_open: bool, line_h: f32) -> El {
    let k = super::key(c.id);
    let open = crate::disclosure::is_open(s, &k, default_open);
    let raw = opencode_kit::model::tool_error(c.metadata).unwrap_or("");
    let cleaned = raw.trim_start().strip_prefix("Error:").unwrap_or(raw).trim().to_owned();
    let id = format!("tool-{}", c.id);
    let title = name(c);
    let shell = matches!(c.name, "shell" | "bash");
    let sub_font = if shell { TOOL_CODE.lh(16.0) } else { UI };
    let sub = subtitle(s, session, c);
    // The Failed tag at the line's end.
    let tag = rune::text::measure(opencode_i18n::t!("ui.toolErrorCard.failed"), &[], UI_MED, None).0 + 12.0;
    // Measured as the shut row lays it out: a subtitle it can't show whole moves into the body.
    let clipped = sub.as_deref().is_some_and(|t| {
        let room = super::lists::column(session) - 14.0 - 16.0 - rune::text::measure(&title, &[], UI, None).0 - 16.0 - 20.0;
        t.contains('\n') || rune::text::measure(t, &[], sub_font, None).0 + tag > room
    });
    // A failed call reads like the other rows (`timeline-next/tools/error-card.tsx`,
    // `basic-tool.css:229`): the tool muted, its subject bright, then a Failed tag, 6 apart.
    let mut words = row().key("words").gap(6.0).center_y().shrink().min_w(0.0).child(text(title.clone()).key("title").font(UI).color(p.muted));
    if let Some(t) = sub.as_deref().filter(|_| !(open && clipped)) {
        let first = t.lines().next().unwrap_or("").to_owned();
        words = words.child(readable(p, text(first).key("subtitle").font(sub_font).color(p.text).truncate_fade()).shrink().min_w(0.0));
    }
    words = words.child(crate::activity::tag(p, opencode_i18n::t!("ui.toolErrorCard.failed"), true).key("failed"));
    let button = row()
        .key(id.clone())
        .h(24.0)
        .center_y()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .role("button")
        .label(opencode_i18n::t!("rune.tool.failed", tool = title))
        .expanded(open)
        .radius(4.0)
        .focus_default(p)
        .hover_motion(INSTANT, INSTANT)
        .on_press_tx(crate::disclosure::set::<S>(k.clone(), open, id.clone()))
        .child(words)
        .child(el().key("arrow").size(14.0, 14.0).ml(6.0).center().child(arrow(p, open).color(p.faint)));
    let mut line = row().key("line").w_full().min_h(line_h).gap(2.0).center_y().child(button);
    if let Some(action) = s.tool_row_action(p, session, c) {
        line = line.child(row().key("actions").child(action));
    }
    // Its Copy fades in and out over 0.15 s ease (`tool-error-card.css:132-140`).
    let card = col().key("error-card").w_full().hover_group_focus().hover_motion(crate::style::HOVER_FADE, crate::style::HOVER_FADE).child(line);
    if !open {
        return card;
    }
    let copy_key = format!("copy-tool-{}-error", c.id);
    let done = crate::copied::is_copied(s, &copy_key);
    let label = if done { opencode_i18n::t!("ui.message.copied") } else { opencode_i18n::t!("ui.toolErrorCard.copyError") };
    let copied = cleaned.clone();
    let copy = row()
        .key("copy")
        .role("button")
        .label(label)
        .help(label)
        .help_open(done)
        .size(24.0, 24.0)
        .center()
        .radius(6.0)
        .focus_default(p)
        .hover(p.hover)
        .press(p.pressed)
        .hover_motion(INSTANT, INSTANT)
        .on_press_tx({
            let copy_key = copy_key.clone();
            move |s: &mut S, tx: &mut Tx| {
                tx.copy(copied.clone());
                crate::copied::mark_copied(s, copy_key.clone(), tx);
            }
        })
        .child(crate::copied::glyph_of(s, &copy_key, 16.0, p.text, &rune_theme_opencode::icons::COPY));
    let copy = crate::style::hover_reveal(copy);
    // `tool-error-card.css:151-172`: a selection in the content is state-bg-danger under text-base,
    // the window active or not.
    let picked = rune::SelectionStyle::new(p.danger_bg).ink(p.text).inactive(p.danger_bg, Some(p.text));
    // `error-details-text` (`packages/ui/src/components/error-details.tsx:7`, `error-details.css:26`,
    // `opencode#65785`): click-focusable for keyboard scrolling (`tab_index(-1)`), no focus ring.
    let mut content = col().key("error").w_full().pr(40.0).focusable(true).tab_index(-1).no_focus_ring();
    if let Some(t) = sub.filter(|_| clipped) {
        // `tool-error-card-subtitle`: a plain `overflow-y: auto` box's bar (`tool-error-card.css:117-122`).
        let full = col().w_full().max_h(super::console::MAX_H).mb(4.0);
        let words = readable(p, text(t).font(if shell { TOOL_CODE } else { UI.lh(20.0) }).color(p.muted).wrap_anywhere()).selection_style(picked).w_full();
        content = content.child(crate::scroller::scroller("subtitle-full").build::<S>(p, full, words));
    }
    content = content.child(readable(p, text(cleaned).font(UI.lh(20.0)).color(p.danger).wrap_anywhere()).selection_style(picked).w_full()).child(copy.pin(None, Some(0.0), Some(0.0), None));
    // The error hangs from the nested line like any other row's content; clicking the rail collapses it.
    card.child(crate::disclosure::reveal(s.knobs(), crate::style::nested_collapsible(p, crate::style::NESTED_GAP, content, crate::disclosure::set::<S>(k, true, id))))
}

/// The width the shut card of `c` takes whole: the warning mark, the name, the subtitle's first
/// line and the error's gist, the arrow, and the gaps between them. A card that lays out inside a
/// row as wide as its content needs it, as its texts would otherwise give way to nothing.
pub fn shut_width<S: TimelineHost>(s: &S, session: &str, c: &Call) -> f32 {
    let raw = opencode_kit::model::tool_error(c.metadata).unwrap_or("");
    let cleaned = raw.trim_start().strip_prefix("Error:").unwrap_or(raw).trim();
    let shell = matches!(c.name, "shell" | "bash");
    let measure = |t: &str, style| rune::text::measure(t, &[], style, None).0;
    let sub = subtitle(s, session, c).map_or(0.0, |t| measure(t.lines().next().unwrap_or(""), if shell { TOOL_CODE.lh(16.0) } else { UI }) + 6.0);
    16.0 + 8.0 + measure(&name(c), UI_MED) + sub + 6.0 + measure(&summary(c.name, cleaned), UI) + 6.0 + 14.0
}

/// The tool's name, as its trigger says it.
fn name(c: &Call) -> String {
    match c.name {
        // Named by what they did rather than the provider (`error-card.tsx`'s `titles`).
        "websearch" => opencode_i18n::t!("ui.tool.searchWeb").to_owned(),
        "webfetch" => opencode_i18n::t!("ui.tool.fetch").to_owned(),
        "multiedit" => crate::turns::tool_title("edit").to_owned(),
        other => crate::turns::tool_title(other).to_owned(),
    }
}

/// What the call was working on, as its own trigger's subtitle says it (`toolErrorSubtitle`),
/// whole: the trigger shows its first line.
fn subtitle<S: TimelineHost>(s: &S, session: &str, c: &Call) -> Option<String> {
    let text = match c.name {
        "shell" | "bash" => field(c.input, "command").or_else(|| field(c.metadata, "command"))?.to_owned(),
        "read" | "edit" | "multiedit" | "write" => file_name(path(c.input)?).to_owned(),
        "list" | "glob" | "grep" => directory(s, session, path(c.input).unwrap_or(s.session_directory(session).unwrap_or("/")))?,
        "webfetch" => field(c.input, "url")?.to_owned(),
        "websearch" => field(c.input, "query")?.to_owned(),
        "skill" => field(c.metadata, "name").or_else(|| field(c.input, "id")).or_else(|| field(c.input, "name"))?.to_owned(),
        "patch" | "apply_patch" => {
            let n = c.metadata["files"].as_array().map_or(0, Vec::len);
            if n == 0 {
                return None;
            }
            files_count(n)
        }
        "question" => match c.input["questions"].as_array().map_or(0, Vec::len) {
            0 => return None,
            n => format!("{n} {}", opencode_i18n::plural_text(opencode_i18n::plural_key!("ui.common.question"), n as u64)),
        },
        "subagent" | "task" => field(c.input, "description")?.to_owned(),
        _ => return None,
    };
    Some(text).filter(|t| !t.is_empty())
}

/// The error's gist: what comes before its first ": ", capitalised, without the tool's own name
/// in front, its lines joined as the summary's `white-space: nowrap` joins them
/// (`tool-error-card.tsx:107`, `tool-error-card.css:37`); "Failed" when there's nothing.
pub fn summary(tool: &str, cleaned: &str) -> String {
    let tool = if tool == "bash" { "shell" } else { tool };
    let tail = cleaned.strip_prefix(&format!("{tool} ")).unwrap_or(cleaned);
    let head = super::plain::one_line(tail.split(": ").next().unwrap_or(""));
    let mut chars = head.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => opencode_i18n::t!("ui.toolErrorCard.failed").to_owned(),
    }
}
