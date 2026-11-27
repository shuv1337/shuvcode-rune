//! Execute: "Execute" and its latest (or failed) nested call; open, a row per nested call
//! (`ExecuteCallRow`), its result (`ExecuteOutputRow`), and a runtime failure under the call that
//! raised it. A View code button beside the trigger opens the script (`ExecuteDetails`).
//!
//! Under `full_codemode` (`anomalyco/opencode#66509`, `tool-renderer.tsx:2110-2460`,
//! `message-part.css:1213-1240`), execute draws in the flat layout: a static muted title locked
//! open over flush-left calls in the UI sans face (with failure warnings after the call name),
//! and `Output` moving after the script's file changes when those changes draw right after it.

use opencode_kit::model::ToolStatus;
use rune::{El, Tx, col, el, icon, row, text};
use serde_json::Value;

use super::console::{capped, console};
use super::plain::{linked, one_line, strip_ansi};
use super::{Call, Ctx, Display, Fold, Press, Trigger, body, disclosure, field, output, sub_key, trigger};
use crate::TimelineHost;
use crate::focus::FocusExt;
use crate::palette::Pal;
use crate::style::{INSTANT, TOOL_CODE, UI, readable};

/// What a script returned, shown as its Output row (`scriptOutputText`, `tool-renderer.tsx:1071`).
/// A failure shows on its failing row instead.
pub fn script_output_text(c: &Call) -> String {
    if error(c).is_some() {
        return String::new();
    }
    strip_ansi(output(c)).replace("\r\n", "\n").replace('\r', "\n").trim().to_owned()
}

pub fn execute<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, c: &Call, after_changes: bool) -> El {
    let calls = calls(c.metadata);
    let error = error(c);
    let out = script_output_text(c);
    let flat = s.full_codemode(cx.session);
    // When its changes are drawn right after it under `full_codemode`, its Output row follows
    // them: steps, changes, then Output (`tool-renderer.tsx:2126`).
    let output_here = !out.is_empty() && !(flat && after_changes);
    let opens = !calls.is_empty() || error.is_some() || output_here;
    let mut d = Display::of(s, cx, c, opens);
    if flat {
        // Flat, the title is static and the calls always show under it: the block never collapses
        // (`tool-renderer.tsx:2155-2167`).
        d.fold = if opens { Fold::Expanded } else { Fold::Collapsed };
    }
    let subtitle = (!flat && !d.open()).then(|| subtitle(&calls)).flatten();
    let t = Trigger { title: opencode_i18n::t!("ui.tool.execute").into(), compact: true, subtitle, code: true, ..Default::default() };
    let press = if opens && !flat { Press::Toggle(super::key(c.id)) } else { Press::None };
    let code = field(c.input, "code").map(str::trim_end).filter(|code| !code.is_empty());
    // The script is whole once the model has written the call, so it can be read while it runs.
    let view_code = code.filter(|_| *c.status != ToolStatus::Pending).map(|_| view_code_button::<S>(p, cx.session, c.id));
    let line = trigger(s, p, cx, c, d, t, press, view_code);
    let build_body = || {
        let owner = calls.iter().rposition(|v| v["status"] == "error");
        // A script can make thousands of calls, so the list shows the latest and each press of "+N
        // more" reveals earlier ones (`executeCallsShown`, `executeCallsRevealed`, opencode#65696).
        let presses = (1..).take_while(|k| s.expanded().contains(&sub_key(c.id, format!("more-{k}")))).count();
        let hidden = calls.len().saturating_sub(CALLS_SHOWN + presses * CALLS_REVEALED);
        let mut list = col().key(calls_key(c.id)).w_full().gap(6.0);
        if hidden > 0 {
            list = list.child(more_button::<S>(p, c.id, presses + 1, hidden, flat));
        }
        for (n, call) in calls.iter().enumerate().skip(hidden) {
            let err = if Some(n) == owner { error.as_deref() } else { None };
            list = list.child(call_row(s, p, cx, c.id, n, Some(call), err, flat));
        }
        // A failure with no call, or one that belongs to a call still hidden above, shows on its own.
        if let (true, Some(e)) = (owner.is_none_or(|o| o < hidden), error.as_deref()) {
            list = list.child(call_row(s, p, cx, c.id, calls.len(), None, Some(e), flat));
        }
        if output_here {
            list = list.child(output_row(s, p, cx.session, c.id, &out, flat));
        }
        // Flat: 6 pt trigger bottom overhang + 2 pt = 8 pt `--execute-title-gap` (`timeline-next.css:503`).
        body(list, false, if flat { 2.0 } else { 4.0 })
    };
    if flat {
        let root = col().w_full().hover_group_focus().child(line);
        if opens { root.child(build_body()) } else { root }
    } else {
        disclosure(s, line, d, build_body)
    }
}

/// How many of a script's latest calls show when it opens, and how many more each "+N more" adds.
const CALLS_SHOWN: usize = 10;
const CALLS_REVEALED: usize = 50;

/// The calls list of script `id`: its own key, so focus can be sent to its first call.
fn calls_key(id: &str) -> String {
    format!("calls-{id}")
}

/// "+N more" above the shown calls, lined up with their names past the markers
/// (`execute-tool-call-more`): faint, brighter under the pointer. Its `press`th press reveals the
/// next calls.
fn more_button<S: TimelineHost>(p: &Pal, id: &str, press: usize, hidden: usize, flat: bool) -> El {
    let label = opencode_i18n::plural!("ui.common.moreCount", hidden);
    let reveal = crate::disclosure::toggle::<S>(sub_key(id, format!("more-{press}")), format!("tool-{id}"));
    let (list, last) = (calls_key(id), hidden <= CALLS_REVEALED);
    row()
        .key("calls-more")
        .role("button")
        .label(label.clone())
        .ml(if flat { 0.0 } else { 20.0 })
        .radius(4.0)
        .cursor(rune::Cursor::Pointer)
        .focus_default(p)
        .hover_group_focus()
        .hover_motion(INSTANT, INSTANT)
        .on_press_tx(move |s: &mut S, tx: &mut Tx| {
            reveal(s, tx);
            // The button leaves with the last hidden call, so focus moves on to the first call
            // rather than nowhere (`reveal` in `tools/tool-renderer.tsx`).
            if last {
                tx.focus(list.clone(), tx.focus_visible());
            }
        })
        .child(text(label).font(TOOL_CODE).color(p.faint).group_color(p.text))
}

/// The nested calls Code Mode reported (`metadata.toolCalls`).
pub fn calls(metadata: &Value) -> Vec<&Value> {
    metadata["toolCalls"].as_array().map(|a| a.iter().filter(|v| v["tool"].is_string()).collect()).unwrap_or_default()
}

/// A runtime failure's message, without its "Error:" prefix; "Failed" when it says nothing.
pub fn error(c: &Call) -> Option<String> {
    let failed = *c.status == ToolStatus::Error || c.metadata["error"] == Value::Bool(true);
    failed.then(|| {
        let raw = opencode_kit::model::tool_error(c.metadata).filter(|e| !e.is_empty() && *c.status == ToolStatus::Error).unwrap_or(output(c));
        let text = strip_ansi(raw);
        let text = text.trim_start().strip_prefix("Error:").unwrap_or(&text).trim().to_owned();
        if text.is_empty() { opencode_i18n::t!("ui.toolErrorCard.failed").to_owned() } else { text }
    })
}

/// "read [path=ci/toolchain.env]": the failed call, else the latest.
fn subtitle(calls: &[&Value]) -> Option<String> {
    let call = calls.iter().find(|v| v["status"] == "error").or(calls.last())?;
    let tool = call["tool"].as_str()?;
    let args = call_args(call);
    Some(if args.is_empty() { tool.to_owned() } else { format!("{tool} [{}]", args.join(", ")) })
}

fn call_args(call: &Value) -> Vec<String> {
    call["input"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| {
            let v = match v {
                Value::String(s) => one_line(s),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            Some(format!("{k}={v}"))
        })
        .collect()
}

/// Execute's `›` marker and arrow: the code font's own glyph, turned down while open.
fn chevron(p: &Pal, open: bool) -> El {
    let g = text("›").font(TOOL_CODE).color(p.faint);
    el().key("arrow").child(g.rotate(if open { std::f32::consts::FRAC_PI_2 } else { 0.0 }).rotate_motion(INSTANT))
}

/// One nested call: a marker, its tool (shimmering while it runs) and args; open, its input and
/// its error. In the flat layout (`full_codemode`), no leading marker is drawn, names use the UI
/// sans face, and a failed call's warning icon sits after its name (`tool-renderer.tsx:2434-2459`).
#[allow(clippy::too_many_arguments)]
fn call_row<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, id: &str, n: usize, call: Option<&Value>, error: Option<&str>, flat: bool) -> El {
    let failed = call.is_none_or(|c| c["status"] == "error");
    let entries = call.and_then(|c| c["input"].as_object());
    let opens = entries.is_some_and(|e| !e.is_empty()) || error.is_some();
    let k = sub_key(id, format!("call-{n}"));
    let open = opens && s.expanded().contains(&k);
    let warn_icon = || icon(&rune_theme_opencode::icons::OUTLINE_HEXAGONAL_WARNING, 14.0).color(p.danger).offset(0.0, 1.0);
    let mut line = row().key(format!("call-{n}")).w_full().gap(6.0).center_y();
    if !flat {
        let marker = if failed { warn_icon() } else { text("›").font(TOOL_CODE).color(p.faint) };
        line = line.child(el().w(14.0).center().child(marker));
    }
    if let Some(c) = call {
        let name = c["tool"].as_str().unwrap_or("");
        let running = c["status"] == "running";
        // Flat names inherit `--line-height-base` (20 px) from `execute-tool-calls` (`message-part.css:352,485-490`).
        let name_font = if flat { UI.lh(20.0) } else { TOOL_CODE };
        let shown = if running && cx.live {
            crate::working::shimmer(s, p, name, name_font, 0.0)
        } else {
            text(name.to_owned()).font(name_font).color(p.text)
        };
        line = line.child(shown.key("name"));
    }
    // Flat: after the name so every call's name lines up with the title; a failure without a call
    // leads with it (`tool-renderer.tsx:2453-2458`).
    if flat && failed {
        line = line.child(el().key("warn").w(14.0).center().child(warn_icon()));
    }
    let summary = match call {
        Some(c) => Some(call_args(c).join(", ")).filter(|a| !a.is_empty()).map(|a| format!("[{a}]")),
        None => error.and_then(|e| e.lines().next()).map(str::to_owned),
    };
    if let Some(sum) = summary.filter(|_| !open) {
        let args = text(sum).key("args").font(TOOL_CODE).color(if call.is_some() { p.muted } else { p.danger }).truncate_fade().shrink().min_w(0.0);
        line = line.child(if opens && call.is_some() { args.group_color(p.text) } else { args });
    }
    if error.is_some() {
        line = line.child(chevron(p, open));
    }
    if opens {
        line = line
            .role("button")
            .expanded(open)
            .radius(4.0)
            .cursor(rune::Cursor::Pointer)
            .focus_default(p)
            .hover_group_focus()
            .hover_motion(INSTANT, INSTANT)
            .on_press_tx(crate::disclosure::toggle::<S>(k, format!("tool-{id}")));
    }
    let mut item = col().key(format!("call-item-{n}")).w_full().child(line);
    if !open {
        return item;
    }
    if let Some(entries) = entries.filter(|e| !e.is_empty()) {
        let mut list = col().key("input").w_full().gap(2.0).pl(13.0);
        for (key, value) in entries {
            let value = value.as_str().map_or_else(|| serde_json::to_string_pretty(value).unwrap_or_default(), str::to_owned);
            list = list.child(
                row().w_full().gap(6.0).child(text(format!("{key}:")).font(TOOL_CODE).color(p.muted))
                    .child(readable(p, text(value).font(TOOL_CODE).color(p.text).wrap_anywhere()).shrink().min_w(0.0)),
            );
        }
        // A hairline down the input's left, 4 pt in from the marker (flush left in flat mode).
        item = item.child(crate::disclosure::reveal(
            s.knobs(),
            row().key("input-box").w_full().mt(4.0).mb(2.0).pl(if flat { 0.0 } else { 4.0 })
                .child(el().key("rule").w(0.5).self_stretch().fill(p.border))
                .child(capped(list).grow(1.0).min_w(0.0)),
        ));
    }
    if let Some(e) = error {
        item = item.child(crate::disclosure::reveal(
            s.knobs(),
            capped(col().key("error").w_full().pl(if flat { 0.0 } else { 20.0 }).mt(4.0).child(readable(p, linked::<S>(p, e, TOOL_CODE, p.danger)))),
        ));
    }
    item
}

/// "Output · 3 lines"; open, a console of the script's result.
/// In the flat layout (`full_codemode`), omits the leading `›` marker, draws "Output" in muted UI
/// sans, and aligns the expanded console flush left (`tool-renderer.tsx:2383-2398`,
/// `message-part.css:1222-1231`).
pub(crate) fn output_row<S: TimelineHost>(s: &S, p: &Pal, session: &str, id: &str, out: &str, flat: bool) -> El {
    let k = sub_key(id, "output");
    let open = s.expanded().contains(&k);
    let lines = out.split('\n').count();
    let mut line = row()
        .key(if flat { format!("output-row-{id}") } else { "output-row".into() })
        .w_full()
        .gap(6.0)
        .center_y()
        .role("button")
        .expanded(open)
        .radius(4.0)
        .cursor(rune::Cursor::Pointer)
        .focus_default(p)
        .hover_group_focus()
        .hover_motion(INSTANT, INSTANT)
        .on_press_tx(crate::disclosure::toggle::<S>(k, format!("tool-{id}")));
    if !flat {
        line = line.child(el().w(14.0).center().child(chevron(p, open)));
    }
    let name_font = if flat { UI.lh(20.0) } else { TOOL_CODE };
    let name_color = if flat { p.muted } else { p.text };
    line = line
        .child(text(opencode_i18n::t!("ui.tool.execute.output")).key("name").font(name_font).color(name_color))
        .child(text(opencode_i18n::plural!("ui.tool.execute.lines", lines)).key("args").font(TOOL_CODE).color(p.muted).group_color(p.text));
    let c = col().key(if flat { format!("script-output-{id}") } else { "output-item".into() }).w_full().child(line);
    if open {
        c.child(crate::disclosure::reveal(
            s.knobs(),
            col().key("output").w_full().pl(if flat { 0.0 } else { 20.0 }).mt(4.0).child(console(s, p, session, id, None, out)),
        ))
    } else {
        c
    }
}

/// View code: a 24 pt ghost icon button that opens the script and its result in a dialog.
fn view_code_button<S: TimelineHost>(p: &Pal, session: &str, id: &str) -> El {
    let open = super::details::key(session, id);
    row()
        .key("view-code")
        .role("button")
        .label_static(opencode_i18n::t!("ui.tool.execute.viewCode"))
        .help(opencode_i18n::t!("ui.tool.execute.viewCode"))
        .size(24.0, 24.0)
        .center()
        .radius(6.0)
        .focus_default(p)
        .hover(p.hover)
        .press(p.pressed)
        .hover_motion(INSTANT, INSTANT)
        .on_press_tx(move |s: &mut S, tx: &mut Tx| {
            tx.instant();
            s.expanded_mut().insert(open.clone());
        })
        .child(crate::style::hover_reveal(icon(&rune_theme_opencode::icons::CODE_SLASH, 14.0).color(p.text)))
}
