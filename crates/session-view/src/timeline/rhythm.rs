//! The timeline's vertical rhythm, as Desktop's New timeline spaces rows
//! (`timeline-next/spacing.ts`, `timeline-next.css`): by who did what. Each thing the user did
//! (a prompt, a command) stands apart, the agent's reply reads as one stretch, and what happened
//! to the session (a switch, a notice) sits together. The room is measured between what a reader
//! sees (a line of text, a card's edge), never between the rows' boxes:
//!
//! | above, below                                                    | gap     |
//! |-----------------------------------------------------------------|---------|
//! | a different block, or either is the user's, or work reporting back | `L` 32 |
//! | the agent's words beside its activity, in a block               | `M` 20  |
//! | two of the same kind (words, or lines of activity), in a block  | `S` 16  |
//!
//! Dividers, compactions, Changed files, a reply's footer and a tool's pictures keep their own
//! room; a row under one of them stands the room it was built with below it. A finished turn a
//! later prompt follows holds its footer under its words (`parts::turn_footer`, shown under the
//! pointer): its room counts toward the gap under them.
//!
//! The upper row of two holds the room between them, under what it shows ([`space_at`]).

use opencode_kit::model::{Message, Part, Transcript};

use super::{RowContent, RowSpec};
use crate::turns::{self, Block};

/// The small gap: between rows of activity in a reply, or a run of session events
/// (`--timeline-gap-s`).
pub const S: f32 = 16.0;
/// The medium gap: between the agent's words and its activity (`--timeline-gap-m`).
pub const M: f32 = 20.0;
/// The large gap: around each thing the user did, and wherever a block starts
/// (`--timeline-gap-l`).
pub const L: f32 = 32.0;
/// A notice's room over it where nothing here spaces it (under a divider or a footer).
pub const NOTICE_PT: f32 = 12.0;

/// Who a row is (`TimelineBlock`); `None` spaces itself.
#[derive(Clone, Copy, PartialEq)]
enum Who {
    User,
    Agent,
    Event,
}

/// What a row shows (`TimelineItem`): the agent's words, or a line of activity.
#[derive(Clone, Copy, PartialEq)]
enum Item {
    Prose,
    Line,
}

/// A row's place in the rhythm: who and what, how far inside its box what shows starts and
/// ends (a 28 pt trigger's 16 pt line sits 6 in; Markdown's trimmed half-leading, prose_insets), whether it
/// is work reporting back, which always starts a block, and the room at its foot a finished
/// turn's footer holds (`parts::turn_footer`).
#[derive(Clone, Copy)]
struct Edges {
    who: Who,
    item: Item,
    top: f32,
    bottom: f32,
    reports: bool,
    /// An `AssistantPart` activity row (`timeline-next/row.tsx:806-816`, `opencode#66509`):
    /// blocks stand `M` (20 pt) apart except before a script's own changes, which keep `S` (16 pt).
    assistant_part: bool,
    /// A `Calls::Files` row beginning with a script's own `Step::Change` (`TimelineRow.scriptChanges`).
    script_changes: bool,
    footer: f32,
}

const fn edges(who: Who, item: Item, inset: f32) -> Edges {
    Edges { who, item, top: inset, bottom: inset, reports: false, assistant_part: false, script_changes: false, footer: 0.0 }
}

/// A 28 pt trigger around a 16 pt line.
const LINE_INSET: f32 = 6.0;
/// Half-leading over and under `md` in points, as Desktop trims it (`timeline-next.css:449-452`,
/// `text-box: trim-both text`): Inter's ascent (1984/2048 em) and descent (494/2048) rounded to
/// device pixels, the leading floored above them and the rest below, as Blink splits it and Rune
/// lays the line out.
fn line_insets(md: rune::TextStyle) -> (f32, f32) {
    let s = crate::scroller::device_scale();
    let line = (md.line_px() * s * 64.0).round() / 64.0;
    let text = (md.size * s * 1984.0 / 2048.0).round() + (md.size * s * 494.0 / 2048.0).round();
    let over = ((line - text) / 2.0).floor();
    (over / s, (line - text - over) / s)
}

/// Markdown's half-leading over and under its prose text at 14/22.4 (`line_insets(MD)`).
/// 2 and 3.4 at 1x; 2.67 and 3.2 at 1.875x.
pub(crate) fn prose_insets() -> (f32, f32) {
    line_insets(crate::style::MD)
}

fn atx_heading(line: &str) -> Option<rune::TextStyle> {
    let s = line.trim_start_matches(' ');
    if line.len() - s.len() > 3 { return None; }
    let n = s.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&n) { return None; }
    let rest = &s[n..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) { return None; }
    Some(crate::style::MD_H[(n - 1).min(3)])
}

/// Top half-leading of `body`'s first line in points when it starts with text (rather than a
/// fenced block), matching `text-box: trim-both text` (`timeline-next.css:425-428`).
pub(crate) fn prose_top_inset(body: &str) -> f32 {
    let first = body.trim().lines().next().unwrap_or("");
    if first.starts_with("```") { 0.0 } else { line_insets(atx_heading(first).unwrap_or(crate::style::MD)).0 }
}

/// Bottom half-leading of `body`'s last line in points when it ends in text (rather than a
/// fenced block, table, rule, or image), matching `text-box: trim-both text` (`timeline-next.css:425-428`).
pub(crate) fn prose_bottom_trim(body: &str) -> f32 {
    let last = body.trim().lines().next_back().unwrap_or("").trim();
    if last.is_empty()
        || last.ends_with("```")
        || last.ends_with("~~~")
        || (last.starts_with('|') && last.ends_with('|'))
        || matches!(last, "---" | "***" | "___")
        || (last.starts_with("![") && last.ends_with(')'))
    {
        return 0.0;
    }
    line_insets(atx_heading(last).unwrap_or(crate::style::MD)).1
}

/// Words; `turn_footer`, they end an earlier turn under its footer (`turn_footer_text`);
/// `keep_bottom`, the row keeps its bottom half-leading (`timeline-next.css:435-438`,
/// `text-box: trim-start text`). Rows without a footer trim their own bottom half-leading via
/// `.mb(-prose_bottom_trim(body))` (`timeline-next.css:425-428`, `text-box: trim-both text`) so
/// `whole_rows()` rounds the trimmed height just as Blink + TanStack Virtual (`virtualizer.tsx:369`) do.
fn prose(body: &str, turn_footer: bool, keep_bottom: bool) -> Edges {
    let footer = if turn_footer { crate::working::COPY_AT.1 + crate::working::COPY } else { 0.0 };
    let top = prose_top_inset(body);
    let bottom = if keep_bottom { prose_bottom_trim(body) } else { 0.0 };
    Edges { top, bottom, footer, ..edges(Who::Agent, Item::Prose, 0.0) }
}

/// The last turn [`edges_of`] folded: a turn's rows sit together, so each turn folds once.
type Folded<'a> = Option<((usize, usize, bool), Vec<Block<'a>>)>;

fn edges_of<'a>(t: &'a Transcript, detail: &crate::detail::Detail, script_diffs: bool, row: &RowContent, folded: &mut Folded<'a>) -> Option<Edges> {
    Some(match row {
        RowContent::User { .. } | RowContent::Pending { .. } => edges(Who::User, Item::Line, 0.0),
        RowContent::Notice(i, ..) => match &t.messages[*i] {
            Message::Notice(n) if crate::turn_errors::is_divider(n) => return None,
            Message::Notice(n) if matches!(n.kind, opencode_kit::model::NoticeKind::Finished(_)) => Edges { reports: true, ..edges(Who::Agent, Item::Line, LINE_INSET) },
            _ => edges(Who::Event, Item::Line, 2.0),
        },
        RowContent::PendingNotice(..) => Edges { reports: true, ..edges(Who::Agent, Item::Line, LINE_INSET) },
        RowContent::Error { .. } | RowContent::Retry { .. } => edges(Who::Agent, Item::Line, 0.0),
        RowContent::TurnText { message, part, live, .. } => match &t.messages[*message] {
            Message::Assistant(a) => match a.parts.get(*part) {
                Some(Part::Text { text }) => {
                    let turn_footer = !live && super::turn_footer_text(t, *message, *part);
                    let keep_bottom = turn_footer || (!live && super::footer_text(t, *message, *part));
                    prose(text, turn_footer, keep_bottom)
                }
                _ => prose("", false, false),
            },
            _ => prose("", false, false),
        },
        RowContent::Turn { start, end, block, live, .. } => {
            let turn = (*start, *end, *live);
            if folded.as_ref().is_none_or(|(at, _)| *at != turn) {
                *folded = Some((turn, turns::fold_with_flags(&super::entries(&t.messages[*start..*end], detail), *live, detail, script_diffs)));
            }
            match folded.as_ref().and_then(|(_, blocks)| blocks.get(*block)) {
                Some(Block::Text { body, msg, part, live, .. }) => {
                    let turn_footer = !live && super::turn_footer_text(t, start + msg, *part);
                    let keep_bottom = turn_footer || (!live && super::footer_text(t, start + msg, *part));
                    prose(body, turn_footer, keep_bottom)
                }
                // Work reporting back starts a block of its own, which the reply continues.
                Some(Block::Events { steps, .. }) => Edges {
                    reports: steps.iter().all(|s| matches!(s, turns::Step::Notice(_))),
                    assistant_part: true,
                    ..edges(Who::Agent, Item::Line, LINE_INSET)
                },
                // A subagent's rows, its reports too, continue the reply (`spacing.ts`'s `reportsBack`
                // counts only events rows).
                Some(Block::Subagent { .. } | Block::Group { .. } | Block::Thought { .. }) => {
                    Edges { assistant_part: true, ..edges(Who::Agent, Item::Line, LINE_INSET) }
                }
                Some(Block::Calls { kind, steps, .. }) => {
                    let top = if *kind == turns::Calls::Files && turns::file_run_head_is_card(steps) { 0.0 } else { LINE_INSET };
                    Edges {
                        top,
                        assistant_part: true,
                        script_changes: *kind == turns::Calls::Files
                            && steps.first().is_some_and(|st| matches!(st, turns::Step::Change { .. })),
                        ..edges(Who::Agent, Item::Line, LINE_INSET)
                    }
                }
                Some(Block::Question { .. }) => Edges { assistant_part: true, ..edges(Who::Agent, Item::Line, 0.0) },
                None => return None,
            }
        }
        _ => return None,
    })
}

/// The gap between two rows (`spacing.ts`'s `gap` + `timeline-next/row.tsx:805-816`):
/// blocks stand `M` apart while a script stands `S` from its own file changes.
fn gap(above: Edges, below: Edges) -> f32 {
    if below.reports || above.who != below.who || below.who == Who::User {
        L
    } else if above.item == below.item {
        if above.assistant_part && !below.script_changes { M } else { S }
    } else {
        M
    }
}

/// The base gap for `Working`'s lane (`spacing.ts`'s `laneGap`, without `row.tsx`'s
/// block-to-block override).
fn lane_gap(above: Edges, below: Edges) -> f32 {
    if below.reports || above.who != below.who || below.who == Who::User {
        L
    } else if above.item == below.item {
        S
    } else {
        M
    }
}

/// Sets the room between each row in `out` and the row above it (see the table above), which
/// the row above holds under what it shows (`RowSpec::under`).
pub(super) fn space(t: &Transcript, detail: &crate::detail::Detail, script_diffs: bool, out: &mut [RowSpec]) {
    space_at(t, detail, script_diffs, out, 0..out.len());
}

/// [`space`] for rows `at` alone, from the row above them: where two spans of a projection meet
/// (`projection`).
///
/// The row directly above holds the room between two rows, never the lower row
/// (`timeline-next/spacing.ts` `timelineGaps`, official `packages/app/AGENTS.md` "Rows that load
/// above the reader", opencode#66014): older history loads above rows a reader is looking at,
/// and a row that took its room on top would grow there when the row before it arrived and move
/// what is on screen. Official gives a turn's first row a turn gap row for it; here the last row
/// of the turn above holds it, which arrives with that turn all the same. The room is whole
/// points, as rows stack (`virtualizer.tsx:362`), so a row is as tall as before with it.
pub(super) fn space_at(t: &Transcript, detail: &crate::detail::Detail, script_diffs: bool, out: &mut [RowSpec], at: std::ops::Range<usize>) {
    let mut folded: Folded = None;
    let mut above: Option<(usize, Option<Edges>)> = out[..at.start.min(out.len())].iter().enumerate().rev()
        .find(|(_, r)| !matches!(r.content, RowContent::Lane(_)))
        .map(|(i, r)| (i, edges_of(t, detail, script_diffs, &r.content, &mut folded)));
    let end = at.end.min(out.len());
    for i in at.start.min(end)..end {
        if matches!(out[i].content, RowContent::Lane(_)) {
            continue;
        }
        let here = edges_of(t, detail, script_diffs, &out[i].content, &mut folded);
        let Some((up, prev)) = above.replace((i, here)) else { continue };
        if let (Some(prev), Some(h)) = (prev, here) {
            let room = (gap(prev, h) - prev.bottom - prev.footer - h.top).max(0.0);
            match &mut out[i].content {
                // Consecutive prompts hold together as a run (`TurnGap`), which spaces them.
                RowContent::User { top, .. } | RowContent::Pending { top, .. } => if prev.who != Who::User && room > 0.0 { *top = room },
                RowContent::Notice(_, _, pt) | RowContent::PendingNotice(_, pt) => *pt = room,
                RowContent::Error { pt, .. } | RowContent::Retry { pt, .. } | RowContent::TurnText { pt, .. } | RowContent::Turn { pt, .. } => *pt = room,
                _ => {}
            }
        }
        // Whatever room the row stands under, the one rhythm set or the one it was built with
        // under a row rhythm doesn't know, goes to the row above.
        if let Some(room) = top_of(&mut out[i].content).map(std::mem::take) {
            out[up].under = room.round();
        }
    }
}

/// The room a row takes over what it shows, where the row above can hold it. Dividers, a tool's
/// pictures and the turn's footer keep their own.
pub(super) fn top_of(row: &mut RowContent) -> Option<&mut f32> {
    match row {
        RowContent::User { top, .. } | RowContent::Pending { top, .. } => Some(top),
        RowContent::Notice(_, _, pt) | RowContent::PendingNotice(_, pt) => Some(pt),
        RowContent::Error { pt, .. } | RowContent::Retry { pt, .. } | RowContent::TurnText { pt, .. } | RowContent::Turn { pt, .. } => Some(pt),
        RowContent::UserImages { pt, .. } | RowContent::TurnDiff { pt, .. } => Some(pt),
        _ => None,
    }
}

/// Extra bottom tail room past the 36 pt Working lane (`tailExtra` in `message-timeline.tsx:226`,
/// `spacing.ts`'s `laneGap`): 0 (`s`), 4 (`m`), or 16 (`l`) so the Working status line keeps its
/// clearance above the floating activity pill.
pub(super) fn tail_extra(t: &Transcript, detail: &crate::detail::Detail, script_diffs: bool, rows: &super::projection::Projection) -> f32 {
    let mut folded: Folded = None;
    let end = (0..rows.len())
        .rev()
        .find(|&i| matches!(rows.get(i), Some(RowSpec { content: RowContent::Actions { .. }, .. })))
        .unwrap_or(rows.len());
    let last = (0..end).rev().find_map(|i| {
        let r = rows.get(i)?;
        if matches!(r.content, RowContent::Lane(_)) {
            return None;
        }
        edges_of(t, detail, script_diffs, &r.content, &mut folded)
    });
    let Some(prev) = last else { return 0.0 };
    (lane_gap(prev, edges(Who::Agent, Item::Line, LINE_INSET)) - S).max(0.0)
}
