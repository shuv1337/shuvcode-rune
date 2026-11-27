//! How a turn's blocks draw (`timeline-next/tools/context-group.tsx`, `assistant-content.tsx`):
//! an activity group, the one thought a group of thoughts alone reads as, an events row, a row
//! of thoughts, and calls on their own row.

use rune::{El, col, el, row, text};

use super::{Calls, Label, Loc, Step, call, file_like, file_tool, grouped_read, label, named_skill, script_change};
use crate::TimelineHost;
use crate::activity::{event_row, events, label as activity};
use crate::detail::Category;
use crate::focus::FocusExt;
use crate::palette::Pal;
use crate::style::{HOVER_FADE, ThoughtPart, UI, arrow, reasoning_at};

/// Between an open group's label and its first step, and between distinct steps: 20 from text to
/// text (`--timeline-gap-m`, `timeline-next.css:453-458,487-494`, `opencode#66509`).
const GROUP_GAP: f32 = crate::timeline::rhythm::M;
/// Between a script and its own file changes inside an open group: 16 (`--timeline-gap-s`).
const SCRIPT_CHANGE_GAP: f32 = crate::timeline::rhythm::S;
/// A 28 pt trigger's empty vertical overhang on each side of its 16 pt line
/// (`timeline-next.css:402` uses `padding-block: 6px; margin-block: -6px`; Rune keeps the 28 pt
/// box and subtracts each row's overhang from the gap between rows).
const TRIGGER_INSET: f32 = 6.0;
/// Solid room below an open group's 28 pt trigger to where its first item's visible content (and
/// any stuck file header) starts: `GROUP_GAP` less the trigger's 6 pt bottom overhang
/// (`timeline-next.css:487-494`, `16 + 6 + 20 = 42` total below `sticky_top`).
const HEADER_PAD: f32 = GROUP_GAP - TRIGGER_INSET;

/// The folded row (`ContextGroup`): its label, and when open, every step in order, 14 in from a
/// line 2 in from its edge (`timeline-next.css`, nested content). Plain reads in a row share a
/// line, edits, writes and patches in a row share one card of files, named skills in a row share
/// one "Loaded" line, and shells reporting back in a row share one nested events row.
/// The label only records what happened: what is live is the status line's (`StatusLane`), so
/// it never shimmers. A group of thoughts alone is one thought row, without a group around it
/// (`context-group.tsx:166-176`), and thoughts one after another in an opened group are one too
/// (`ThoughtRun`); one of shell reports alone is the events row it is. The row springs open and
/// shut on the timeline's growth, the timeline holding as Desktop's does
/// (`disclosure::toggle_group`), and while open the header sticks to the timeline's top until
/// the group scrolls past. `live`: the turn is still running, so a step still under way
/// shimmers.
#[allow(clippy::too_many_arguments)]
pub fn group<S: TimelineHost>(
    s: &S,
    p: &Pal,
    session: &str,
    key: &str,
    steps: &[Step],
    at: &[Loc],
    turn: std::ops::Range<usize>,
    live: bool,
    fade: f32,
    extra_after: &std::collections::HashSet<&str>,
) -> El {
    let open = s.expanded().contains(key);
    if steps.iter().all(|st| matches!(st, Step::Thought { .. })) {
        let foot = thought_foot(s, live, false, steps);
        return thoughts(s, p, steps, live, false, fade).pb((TRIGGER_INSET - foot).max(0.0));
    }
    let work = activity::work_events(steps);
    let work_only = !work.is_empty() && steps.iter().all(is_work);
    if work_only {
        return event_row::view(
            s,
            p,
            session,
            &work,
            open,
            crate::disclosure::toggle_group::<S>(key.to_owned()),
            crate::disclosure::collapse_group::<S>(key.to_owned()),
        );
    }
    let shutting = !open && crate::disclosure::is_shutting(s, key);
    let named = activity::activity_with(steps, s.full_codemode(session));
    let words_text = named.as_ref().map_or_else(|| label(steps).text(), activity::Activity::text);
    let mut trigger = row()
        .key("trigger")
        .role("button")
        .label(words_text)
        .expanded(open)
        .h(28.0)
        .gap(4.0)
        .center_y()
        .radius(4.0)
        .focus_default(p)
        .hover_group_focus()
        // The arrow fades in under the pointer (`collapsible.css:26-29`).
        .hover_motion(HOVER_FADE, HOVER_FADE)
        // A step joining or the label changing crossfades the words while the arrow slides over.
        .animate_layout()
        .on_press_tx(crate::disclosure::toggle_group::<S>(key.to_owned()));
    // A card of files or an expanded thought in the group holds headers that stick under this one
    // (Desktop's `--sticky-accordion-top`, `message-part.css:1330-1332`, `opencode#65246`).
    let files = steps.iter().any(|st| file_tool(st) || matches!(st, Step::Change { file, .. } if crate::tools::script::is_card(file)));
    let open_thought = (open || shutting) && has_open_thought(s, live, steps);
    if open || shutting {
        trigger = trigger.fill(p.bg_base).sticky_top(crate::style::sticky_top());
        trigger = if files || open_thought {
            // A solid strip under the label, which a stuck file or thought header meets; the soft
            // edge would wash over that header. Without files, stop at a nested thought trigger's
            // top edge (`TRIGGER_INSET` higher than a bordered card) so its focus ring clears.
            let pad_h = if files { HEADER_PAD } else { (HEADER_PAD - TRIGGER_INSET).max(0.0) };
            trigger.child(el().key("pad").pin(Some(0.0), Some(28.0), Some(0.0), None).h(pad_h).fill(p.bg_base))
        } else {
            // Steps scroll under it and soften into its fill rather than meeting a hard edge, and
            // shutting from there they slide away under it.
            trigger.stuck_shadow(rune::Shadow { y: 10.0, blur: 20.0, color: p.bg_base, ..Default::default() })
        };
    }
    trigger = match named {
        Some(a) => trigger.child(activity_words(p, &a)),
        None => trigger.child(used_words(p, &label(steps))),
    };
    // The arrow shows while the pointer is over the label, and always while open.
    let a = el().key("arrow").w(16.0).h(24.0).ml(2.0).center().child(arrow(p, open));
    trigger = trigger.child(if open || crate::style::hover_none() { a } else { a.group_opacity(0.0, 1.0) });
    let mut c = col().w_full().child(trigger);
    if open || shutting {
        // A file header in the steps sticks under the label and the gap under it.
        let top = crate::style::sticky_top();
        // The steps stack as growth rows on the fold's spring, so a step folding open or shut
        // in the middle slides the ones under it rather than moving them at once (t1484).
        // They're virtual rows: an open group of a thousand calls builds the ones in view, each
        // found again from the transcript by its place (`Loc`), never the whole turn's fold.
        let fold = crate::disclosure::fold(s.knobs());
        let edge = rune::StreamEdge { word_fade: 0.0, ..rune::edge::FEATHER };
        let items = std::rc::Rc::new(plan(steps));
        let flat = s.full_codemode(session);
        let placement = super::script_output_for_steps(steps, flat);
        let mut after_refs: std::collections::HashSet<&str> = placement.after_changes.clone();
        after_refs.extend(extra_after.iter().copied());
        let edges: Vec<(f32, f32)> = items
            .iter()
            .map(|&(i, n)| {
                let slice = &steps[i..i + n];
                (step_head(slice), step_foot(s, session, live, slice, &after_refs, &placement.output_after))
            })
            .collect();
        let first_head = edges.first().map_or(TRIGGER_INSET, |&(h, _)| h);
        let last_foot = edges.last().map_or(TRIGGER_INSET, |&(_, f)| f);
        let top_gap = (GROUP_GAP - TRIGGER_INSET - first_head).max(0.0);
        let row_gaps: std::rc::Rc<Vec<f32>> = std::rc::Rc::new(
            items
                .iter()
                .enumerate()
                .map(|(j, &(i, _))| {
                    if j == 0 {
                        return 0.0;
                    }
                    let target = if matches!(steps.get(i), Some(Step::Change { .. })) {
                        SCRIPT_CHANGE_GAP
                    } else {
                        GROUP_GAP
                    };
                    (target - edges[j - 1].1 - edges[j].0).max(0.0)
                })
                .collect(),
        );
        let pb = (TRIGGER_INSET - last_foot).max(0.0);
        let rail_top = top_gap + first_head;
        let rail_bottom = pb + last_foot;
        let after_ids: std::rc::Rc<std::collections::HashSet<String>> = std::rc::Rc::new(
            after_refs.into_iter().map(str::to_owned).collect(),
        );
        let output_after: std::rc::Rc<std::collections::HashMap<String, (String, String)>> = std::rc::Rc::new(
            placement.output_after.into_iter().map(|(k, (sid, text))| (k, (sid.to_owned(), text))).collect(),
        );
        let keys: std::rc::Rc<Vec<String>> = std::rc::Rc::new(items.iter().map(|&(i, _)| item_key(steps, i)).collect());
        let at: std::rc::Rc<[Loc]> = at.into();
        let version = (key, &*keys, flat);
        let (count, session, p, group_key) = (items.len(), session.to_owned(), *p, key.to_owned());
        let (row_keys, span) = (keys.clone(), turn);
        let list = col().key("rows").w_full().transition(rune::Transition::none()).growth(rune::GrowthSpec { spring: fold, edge })
            .virtual_rows(count, 36.0, move |_: &S, j| row_keys[j].clone().into(), move |s: &S, j| {
                // Rows build after the group's row returns, maybe beside another timeline's.
                crate::style::set_sticky_top(top);
                let (i, n) = items[j];
                let built = s.with_transcript(&session, |t| {
                    let messages = t.messages.get(span.clone())?;
                    let steps: Vec<Step> = at.get(i..i + n)?.iter().map(|l| l.step(messages)).collect::<Option<_>>()?;
                    let cx = crate::tools::Ctx { session: &session, live, sticky: top + 28.0 + HEADER_PAD };
                    Some(step_item(s, &p, cx, &group_key, &steps, live, fade, &after_ids, &output_after))
                }).flatten().unwrap_or_else(col);
                // The gap is the row's own, so the list holds rows alone.
                let feather = s.knobs().get(&crate::disclosure::FOLD_FEATHER);
                col().w_full().pt(row_gaps[j]).child(built).fold_glide(crate::disclosure::fold(s.knobs())).fold_feather(feather)
            })
            .virtual_rows_version(version);
        // Rail only collapses open groups; while `shutting` (`!open`) it stays non-interactive so
        // clicking or double-clicking mid-fold never re-opens the group (`nested-rail.ts`).
        let rail = if open {
            crate::style::nested_rail(&p, rail_top, rail_bottom, crate::disclosure::collapse_group::<S>(key.to_owned()))
        } else {
            let line = p.border.alpha(p.border.a * 0.5);
            el().key("rail").pin(Some(2.0), Some(rail_top), None, Some(rail_bottom)).w(1.0).fill(line)
        };
        let list = col().key("steps").w_full().pt(top_gap).pb(pb).pl(14.0).transition(rune::Transition::none()).child(rail).child(list);

        // Shutting, the steps stay laid out below a row that's only the header, and the row's
        // clip folds them away as its height springs down to it.
        // The box keeps the steps' path the same either way, so they don't remount on a press.
        let held = col().key("steps-box").w_full().transition(rune::Transition::none());
        c = c.child(if open { held } else { held.h(0.0) }.child(list));
    }
    c
}

/// An activity label's words (`context-tool-group-title[data-activity]`): the verb muted, what it
/// touched in base ink, then a dot and the rest, 6 apart, all at 440.
fn activity_words(p: &Pal, a: &activity::Activity) -> El {
    let mut r = row().key("words").gap(6.0).center_y().shrink().min_w(0.0).child(text(a.verb.clone()).key("verb").font(UI).color(p.muted));
    if !a.main.is_empty() {
        r = r.child(text(a.main.clone()).key("main").font(UI).color(p.text).shrink().min_w(0.0).truncate_fade());
    }
    if !a.others.is_empty() {
        r = r.child(text("\u{00b7}").key("sep").font(UI).color(p.muted))
            .child(text(a.others.join(", ")).key("others").font(UI).color(p.text).shrink().min_w(0.0).truncate_fade());
    }
    r
}

/// "Used 2 Thoughts" or "Updates" (`UsedTitle`).
fn used_words(p: &Pal, l: &Label) -> El {
    let words = |key: &'static str, words: &str| (!words.is_empty()).then(|| text(words.to_owned()).key(key).font(UI).color(p.muted));
    row().key("words").gap(4.0).center_y().shrink().min_w(0.0)
        .children(words("used", &l.before))
        .children(l.count.map(|n| text(n.to_string()).key("count").font(UI.features("'tnum' 1")).color(p.text)))
        .children(words("between", &l.between))
        .children((!l.title.is_empty()).then(|| text(l.title.clone()).key("title").font(UI).color(p.text).truncate_fade()))
        .children(words("after", &l.after))
}

/// A step that is work running on, in an opened group: a background shell reporting back
/// (`ContextGroup`'s `WorkRun`). Subagents never join a group.
fn is_work(st: &Step) -> bool {
    matches!(st, Step::Notice(n) if events::notice_event(n).is_some())
}

/// An open group's rows: each the step at its first index and how many steps it draws (plain
/// reads, file edits, named skills and work running on in a row share one).
fn plan(steps: &[Step]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < steps.len() {
        let run = |f: fn(&Step) -> bool| steps[i..].iter().take_while(|st| f(st)).count();
        let n = match &steps[i] {
            st if is_work(st) => run(is_work),
            Step::Thought { .. } => run(|st| matches!(st, Step::Thought { .. })),
            st @ Step::Tool { .. } if grouped_read(st) => run(grouped_read),
            st if file_like(st) => run(file_like),
            st @ Step::Tool { .. } if named_skill(st) => run(named_skill),
            _ => 1,
        };
        out.push((i, n));
        i += n;
    }
    out
}

/// The key of the group row whose first step is `steps[i]`, as [`step_item`] keys it.
fn item_key(steps: &[Step], i: usize) -> String {
    match &steps[i] {
        st if is_work(st) => format!("work-{i}"),
        Step::Thought { msg, index, .. } => format!("r-{msg}-{index}"),
        Step::Notice(n) => format!("notice-{}", n.id),
        Step::Text { .. } => format!("text-{i}"),
        st @ Step::Tool { id, .. } if grouped_read(st) => format!("reads-{id}"),
        st if file_like(st) => format!("files-{}", file_key(st)),
        st @ Step::Tool { id, .. } if named_skill(st) => format!("skills-{id}"),
        Step::Tool { id, .. } => format!("step-{id}"),
        Step::Change { .. } => format!("files-{}", file_key(&steps[i])),
    }
}

/// Events in a run of work (`ContextGroup`'s `items`): shells reporting back, in order.
fn run_events(steps: &[Step]) -> Vec<events::Event> {
    steps.iter().filter_map(activity::step_event).collect()
}

/// Whether a `file_like` step draws a bordered file card (0 pt overhang) rather than a 28 pt
/// trigger or `counts_row` (6 pt overhang).
fn file_step_is_card(st: &Step) -> bool {
    match st {
        Step::Change { file, .. } => crate::tools::script::is_card(file),
        Step::Tool { .. } => call(st).is_some_and(|c| crate::tools::file_draws_card(&c)),
        _ => false,
    }
}

/// Whether a run of `file_like` steps starts with a bordered card (0 pt top overhang).
pub fn file_run_head_is_card(steps: &[Step]) -> bool {
    let Some(first) = steps.first() else { return false };
    if script_change(first) {
        file_step_is_card(first)
    } else {
        steps.iter().take_while(|st| !script_change(st)).any(file_step_is_card)
    }
}

/// Whether a run of `file_like` steps ends with a bordered card or a flat `Output` row (0 pt
/// trigger bottom overhang).
fn file_run_foot_is_flush(steps: &[Step], output_after: &std::collections::HashMap<String, (&str, String)>) -> bool {
    let Some(last) = steps.last() else { return false };
    if let Step::Change { msg, script, index, .. } = last
        && output_after.get(&crate::tools::script::change_id(msg, script, *index)).is_some_and(|(_, t)| !t.is_empty())
    {
        return true;
    }
    if script_change(last) {
        file_step_is_card(last)
    } else {
        steps.iter().rev().take_while(|st| !script_change(st)).any(file_step_is_card)
    }
}

/// Whether a thought run is expanded with non-empty text (and thus shows its body and pins its
/// sticky header).
fn thought_is_open<S: TimelineHost>(s: &S, live: bool, thinking: bool, steps: &[Step]) -> bool {
    let Some(Step::Thought { msg, index, .. }) = steps.first() else { return false };
    let has_details = steps.iter().any(|st| matches!(st, Step::Thought { body, .. } if !body.trim().is_empty()));
    let thinking = thinking || (live && unfinished(steps));
    has_details && crate::disclosure::is_open(s, &format!("r-{msg}-{index}"), thinking && s.timeline_detail().expanded(Category::Thinking))
}

/// Whether any run of thoughts in `steps` is expanded with non-empty text.
fn has_open_thought<S: TimelineHost>(s: &S, live: bool, steps: &[Step]) -> bool {
    let mut i = 0;
    while i < steps.len() {
        if matches!(steps[i], Step::Thought { .. }) {
            let n = steps[i..].iter().take_while(|st| matches!(st, Step::Thought { .. })).count();
            if thought_is_open(s, live, false, &steps[i..i + n]) {
                return true;
            }
            i += n;
        } else {
            i += 1;
        }
    }
    false
}

/// Visual bottom overhang of a thought row (`TRIGGER_INSET` while shut, prose bottom half-leading
/// while open).
fn thought_foot<S: TimelineHost>(s: &S, live: bool, thinking: bool, steps: &[Step]) -> f32 {
    if thought_is_open(s, live, thinking, steps) { crate::timeline::rhythm::prose_insets().1 } else { TRIGGER_INSET }
}

/// How far inside its box an open group's row visually starts (`TRIGGER_INSET` for a 28 pt
/// trigger, `0.0` for a bordered file card, top half-leading for commentary prose).
fn step_head(steps: &[Step]) -> f32 {
    let Some(first) = steps.first() else { return TRIGGER_INSET };
    match first {
        st if is_work(st) => TRIGGER_INSET,
        Step::Thought { .. } | Step::Notice(_) => TRIGGER_INSET,
        Step::Text { body } => crate::timeline::rhythm::prose_top_inset(body),
        st @ Step::Tool { .. } if grouped_read(st) || named_skill(st) => TRIGGER_INSET,
        _ if file_like(first) => if file_run_head_is_card(steps) { 0.0 } else { TRIGGER_INSET },
        Step::Tool { .. } => TRIGGER_INSET,
        Step::Change { .. } => if file_step_is_card(first) { 0.0 } else { TRIGGER_INSET },
    }
}

/// How far inside its box an open group's row visually ends (`TRIGGER_INSET` for a closed 28 pt
/// trigger; `0.0` for a card, flat/open execute, or open tool body; bottom half-leading for open
/// thought or commentary prose).
fn step_foot<S: TimelineHost>(
    s: &S,
    session: &str,
    live: bool,
    steps: &[Step],
    after_ids: &std::collections::HashSet<&str>,
    output_after: &std::collections::HashMap<String, (&str, String)>,
) -> f32 {
    let Some(first) = steps.first() else { return TRIGGER_INSET };
    match first {
        st if is_work(st) => TRIGGER_INSET,
        Step::Thought { .. } => thought_foot(s, live, false, steps),
        Step::Notice(_) => TRIGGER_INSET,
        Step::Text { body } => crate::timeline::rhythm::prose_bottom_trim(body),
        st @ Step::Tool { .. } if grouped_read(st) || named_skill(st) => TRIGGER_INSET,
        _ if file_like(first) => if file_run_foot_is_flush(steps, output_after) { 0.0 } else { TRIGGER_INSET },
        st @ Step::Tool { id, .. } => {
            let cx = crate::tools::Ctx { session, live, sticky: 0.0 };
            if call(st).is_some_and(|c| crate::tools::shows_body_with(s, cx, &c, after_ids.contains(*id))) { 0.0 } else { TRIGGER_INSET }
        }
        Step::Change { .. } => if file_run_foot_is_flush(steps, output_after) { 0.0 } else { TRIGGER_INSET },
    }
}

/// Visual bottom overhang of a standalone `Block::Calls` row (`rhythm` expects `TRIGGER_INSET`).
fn calls_foot<S: TimelineHost>(
    s: &S,
    session: &str,
    live: bool,
    kind: Calls,
    steps: &[Step],
    after_changes: bool,
    output_after: &std::collections::HashMap<String, (&str, String)>,
) -> f32 {
    match kind {
        Calls::Reads => TRIGGER_INSET,
        Calls::Files => if file_run_foot_is_flush(steps, output_after) { 0.0 } else { TRIGGER_INSET },
        Calls::One => {
            let Some(first) = steps.first() else { return TRIGGER_INSET };
            let has_images = matches!(first, Step::Tool { files, .. } if !files.is_empty());
            if has_images {
                return 0.0;
            }
            let cx = crate::tools::Ctx { session, live, sticky: 0.0 };
            if call(first).is_some_and(|c| crate::tools::shows_body_with(s, cx, &c, after_changes)) { 0.0 } else { TRIGGER_INSET }
        }
    }
}

/// One open group's row, `steps` those it draws ([`plan`]).
#[allow(clippy::too_many_arguments)]
fn step_item<S: TimelineHost>(
    s: &S,
    p: &Pal,
    cx: crate::tools::Ctx,
    group: &str,
    steps: &[Step],
    live: bool,
    fade: f32,
    after_ids: &std::collections::HashSet<String>,
    output_after: &std::collections::HashMap<String, (String, String)>,
) -> El {
    let Some(first) = steps.first() else { return col() };
    match first {
        st if is_work(st) => {
            let key = format!("{group}:events:{}", item_key(steps, 0));
            let open = s.expanded().contains(&key);
            col().key(format!("work-{key}")).w_full().child(event_row::view(
                s,
                p,
                cx.session,
                &run_events(steps),
                open,
                crate::disclosure::set::<S>(key.clone(), open, String::new()),
                crate::disclosure::set::<S>(key.clone(), true, String::new()),
            ))
        }
        // A 28 pt thought trigger has `TRIGGER_INSET` (6 pt) top overhang above its 16 pt line, so
        // subtracting it aligns the stuck trigger with its resting spot `GROUP_GAP` under the group
        // header with zero jump.
        Step::Thought { .. } => thoughts_at(s, p, steps, live, false, fade, (cx.sticky - TRIGGER_INSET).max(0.0)),
        // 28 high like the steps around it (`py-1`).
        Step::Notice(n) => col().key(format!("notice-{}", n.id)).w_full().py(4.0).child(crate::turn_errors::notice(s, p, n, crate::compaction::Run::default())),
        // Commentary is keyed by its place among the group's steps, as it has no id.
        Step::Text { body } => col().w_full().child(crate::timeline::markdown_view::<S>(p, body, fade, false)),
        st @ Step::Tool { .. } if grouped_read(st) => {
            let calls: Vec<crate::tools::Call> = steps.iter().filter_map(call).collect();
            crate::tools::cached(s, p, cx, &calls, || crate::tools::read_group(s, p, cx, &calls)).key(format!("reads-{}", calls[0].id))
        }
        st if file_like(st) => file_runs_owned(s, p, cx, steps, output_after),
        st @ Step::Tool { .. } if named_skill(st) => {
            let calls: Vec<crate::tools::Call> = steps.iter().filter_map(call).collect();
            col().key(format!("skills-{}", calls[0].id)).w_full().child(crate::tools::cached(s, p, cx, &calls, || crate::tools::skill_group(s, p, cx, &calls)))
        }
        Step::Tool { id, name, status, input, metadata, .. } => {
            // A group shows its calls' images once under the group (`RowContent::ToolImages`),
            // not duplicated inside each grouped step (`tool-display.tsx:42`, `context-group.tsx:350`).
            let c = crate::tools::Call { id, name, status, input, metadata };
            let after_changes = after_ids.contains(*id);
            col().key(format!("step-{id}")).w_full().child(crate::tools::view_with(s, p, cx, &c, after_changes))
        }
        Step::Change { .. } => file_runs_owned(s, p, cx, steps, output_after),
    }
}

/// Whether the model hasn't finished one of the thoughts in `steps`.
fn unfinished(steps: &[Step]) -> bool {
    steps.iter().any(|st| matches!(st, Step::Thought { secs: None, .. }))
}

/// Thoughts one after another (`ThoughtRow`, anomalyco/opencode#66349): "Thought" and how long
/// they took together, opening to each in turn; one thought is the same row. Settled, it starts
/// shut. It reads Thinking and streams in view where Timeline detail expands thinking
/// (`thoughtOpens`) while the model is writing one of them (`live`, the turn runs), or while the
/// row stands at the end of a running turn (`thinking`: `TimelineRow.Thinking`). Its first
/// thought's key holds it open or shut (`row.tsx`: `group.refs[0].partID`), in a group and on
/// its own row alike.
pub fn thoughts<S: TimelineHost>(s: &S, p: &Pal, steps: &[Step], live: bool, thinking: bool, fade: f32) -> El {
    thoughts_at(s, p, steps, live, thinking, fade, crate::style::sticky_top())
}

fn thoughts_at<S: TimelineHost>(s: &S, p: &Pal, steps: &[Step], live: bool, thinking: bool, fade: f32, sticky: f32) -> El {
    let Some(Step::Thought { msg, index, .. }) = steps.first() else { return col() };
    let key = format!("r-{msg}-{index}");
    let thinking = thinking || (live && unfinished(steps));
    let parts: Vec<ThoughtPart> = steps
        .iter()
        .filter_map(|st| match st {
            Step::Thought { body, secs, .. } => Some(ThoughtPart { body: *body, streaming: live && secs.is_none() }),
            _ => None,
        })
        .collect();
    // What they took together, added up before it's rounded (`thought.tsx:24-31`); none until
    // every one is finished.
    let took: Option<i64> = steps
        .iter()
        .map(|st| match st {
            Step::Thought { took_ms, .. } => *took_ms,
            _ => None,
        })
        .sum();
    let secs = took.filter(|_| !thinking).map(|ms| (ms as f64 / 1000.0).round() as i64);
    let open = crate::disclosure::is_open(s, &key, thinking && s.timeline_detail().expanded(Category::Thinking));
    reasoning_at::<S>(s, p, &key, &parts, secs, thinking, open, fade, sticky).key(key)
}

/// An events row on its own (`TimelineEventRow`), keyed `key`: subagents and background shells
/// launched together, or reporting back one after another.
pub fn events<S: TimelineHost>(s: &S, p: &Pal, session: &str, key: &str, steps: &[Step]) -> El {
    let open = s.expanded().contains(key);
    let list: Vec<events::Event> = steps.iter().filter_map(activity::step_event).collect();
    event_row::view(
        s,
        p,
        session,
        &list,
        open,
        crate::disclosure::toggle_group::<S>(key.to_owned()),
        crate::disclosure::collapse_group::<S>(key.to_owned()),
    )
}

/// Renders a non-text [`super::Block`] at `block` inside `blocks` with cross-block script output
/// placement (`opencode#66509`).
#[allow(clippy::too_many_arguments)]
pub fn block_row<S: TimelineHost>(
    s: &S,
    p: &Pal,
    session: &str,
    blocks: &[super::Block<'_>],
    block: usize,
    turn: std::ops::Range<usize>,
    live: bool,
    fade: f32,
    pt: f32,
    flat: bool,
) -> El {
    let placement = super::script_output_for_blocks(blocks, flat);
    match blocks.get(block) {
        Some(super::Block::Group { key, steps, at }) => group(s, p, session, key, steps, at, turn, live, fade, &placement.after_changes).pt(pt),
        Some(super::Block::Events { key, steps }) => events(s, p, session, key, steps).pt(pt),
        Some(super::Block::Subagent { step, .. }) => subagent(s, p, session, step).pt(pt),
        Some(super::Block::Question { id, status, input, metadata }) => crate::question_row::view(s, p, session, id, status, input, metadata).pt(pt),
        Some(super::Block::Thought { steps, live: thinking, .. }) => {
            let foot = thought_foot(s, live, live && *thinking, steps);
            thoughts(s, p, steps, live, live && *thinking, fade).pt(pt).pb((TRIGGER_INSET - foot).max(0.0))
        }
        Some(super::Block::Calls { kind, steps, .. }) => {
            let after = *kind == Calls::One
                && matches!(steps.first(), Some(Step::Tool { id, .. }) if placement.after_changes.contains(*id));
            let foot = calls_foot(s, session, live, *kind, steps, after, &placement.output_after);
            calls(s, p, session, *kind, steps, live, after, &placement.output_after).pt(pt).pb((TRIGGER_INSET - foot).max(0.0))
        }
        _ => col(),
    }
}

/// Calls on their own row (`AssistantContent`, `SessionReadToolGroup`, `FileGroup`), drawn as
/// they draw in a group: one call, plain reads sharing a line, or edits sharing a file card.
#[allow(clippy::too_many_arguments)]
pub fn calls<S: TimelineHost>(
    s: &S,
    p: &Pal,
    session: &str,
    kind: Calls,
    steps: &[Step],
    live: bool,
    after_changes: bool,
    output_after: &std::collections::HashMap<String, (&str, String)>,
) -> El {
    let cx = crate::tools::Ctx { session, live, sticky: crate::style::sticky_top() };
    if kind == Calls::Files {
        return file_runs(s, p, cx, steps, output_after);
    }
    let calls: Vec<crate::tools::Call> = steps.iter().filter_map(call).collect();
    let Some(first) = calls.first() else { return col() };
    match kind {
        Calls::Reads => crate::tools::cached(s, p, cx, &calls, || crate::tools::read_group(s, p, cx, &calls)).key(format!("reads-{}", first.id)),
        Calls::Files => col(),
        Calls::One => {
            let files = match &steps[0] {
                Step::Tool { files, .. } => *files,
                _ => &[],
            };
            col().key(format!("step-{}", first.id)).w_full().child(crate::tools::view_with(s, p, cx, first, after_changes))
                .children(crate::images::gallery(s, p, &format!("step-images-{}", first.id), files, false, 22.0, None).map(|g| g.pl(22.0)))
        }
    }
}

/// A subagent's event on its row (`timeline-subagent`): nothing while its call is still being
/// written.
pub fn subagent<S: TimelineHost>(s: &S, p: &Pal, session: &str, step: &Step) -> El {
    activity::step_event(step).map_or_else(col, |e| event_row::subagent(s, p, session, &e))
}

/// What keys a row of files: its first call's id, or its first change's.
fn file_key(st: &Step) -> String {
    match st {
        Step::Change { msg, script, index, .. } => crate::tools::script::change_id(msg, script, *index),
        Step::Tool { id, .. } => (*id).to_owned(),
        _ => String::new(),
    }
}

/// Edits and a script's changes in one row, as runs in the order they happened
/// (`session-ui/src/tools/file-runs.ts`'s `fileRuns`, `FileChangeRuns`): calls one after another
/// share one card of files, and each script's changes follow as its own cards
/// ([`crate::tools::script::changes`]).
fn file_runs_owned<S: TimelineHost>(
    s: &S,
    p: &Pal,
    cx: crate::tools::Ctx,
    steps: &[Step],
    output_after: &std::collections::HashMap<String, (String, String)>,
) -> El {
    let borrowed: std::collections::HashMap<String, (&str, String)> =
        output_after.iter().map(|(k, (sid, txt))| (k.clone(), (sid.as_str(), txt.clone()))).collect();
    file_runs(s, p, cx, steps, &borrowed)
}

fn file_runs<S: TimelineHost>(
    s: &S,
    p: &Pal,
    cx: crate::tools::Ctx,
    steps: &[Step],
    output_after: &std::collections::HashMap<String, (&str, String)>,
) -> El {
    let Some(first) = steps.first() else { return col() };
    let mut out = col().key(format!("files-{}", file_key(first))).w_full().gap(crate::tools::script::CARD_GAP);
    let mut i = 0;
    while i < steps.len() {
        let changes = script_change(&steps[i]);
        let n = steps[i..].iter().take_while(|st| script_change(st) == changes).count();
        let run = &steps[i..i + n];
        out = out.child(if changes {
            let list: Vec<(&str, &str, usize, &serde_json::Value)> = run.iter().filter_map(|st| match st {
                Step::Change { msg, script, index, file } => Some((*msg, *script, *index, *file)),
                _ => None,
            }).collect();
            crate::tools::script::changes(s, p, cx, &list, output_after).key(format!("run-{}", file_key(&run[0])))
        } else {
            let calls: Vec<crate::tools::Call> = run.iter().filter_map(call).collect();
            col().key(format!("run-{}", file_key(&run[0]))).w_full().child(crate::tools::cached(s, p, cx, &calls, || crate::tools::file_group(s, p, cx, &calls)))
        });
        i += n;
    }
    out
}
