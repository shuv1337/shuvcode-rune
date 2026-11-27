//! A turn's work as Desktop's New timeline projects it (`session-ui/src/timeline-next/projection.ts`:
//! `turnRows`, `groupContent`, `groupNotices`; `rules.ts`: `contentShows`, `contentGroup`), by
//! Settings › Preferences › Timeline detail (`detail.rs`).
//!
//! Each kind of activity stands on its own row, folds into the turn's activity group, or doesn't
//! show, a failure included. What folds gathers across a turn's steps, up to the next row of its
//! own: thoughts, tool calls, notices and, where commentary folds, the text a later tool call
//! follows. Each subagent event, a launch, a follow-up or a report, is a row of its own in
//! timeline order, never counted (anomalyco/opencode#64017). Background shells are events: where
//! their setting groups them their launch joins the group, else launches one after another share
//! one events row ("Started 2 shells"), as reports one after another do; a report right after the
//! group that launched it finishes there. The thought the model is writing stands at
//! the turn's end as Thinking; finished, it settles where thoughts go. A group names what the
//! agent did (`activity::label`); one of thoughts alone is those thoughts, without a group.
//! Thoughts one after another with nothing between read as one thought (anomalyco/opencode#66349),
//! and while the turn runs the thoughts it ends on read as Thinking until something else shows.

mod view;

pub use view::{block_row, calls, events as events_row, file_run_head_is_card, group, subagent as subagent_row, thoughts};

use opencode_kit::model::{Assistant, Attachment, Notice, Part, ToolStatus};

use crate::activity::{events, rules};
use crate::detail::{Category, Detail, Placement};

/// One entry of a turn: a step, or a notice that folds with its work.
#[derive(Clone, Copy, Debug)]
pub enum Entry<'a> {
    Step(&'a Assistant),
    Notice(&'a Notice),
    /// A message the turn passes over without showing (a hidden notice).
    Skip,
    /// The run ended (an idle marker), unshown: a wake-up (a watch, a subagent, a job) can start
    /// another run in the same turn, and each run keeps its own answer ([`commentary`]).
    Idle,
}

/// One step inside a group, or on its own row.
#[derive(Debug)]
pub enum Step<'a> {
    /// `took_ms`: how long it took once finished, which a row of several adds up before rounding.
    Thought { msg: &'a str, index: usize, body: &'a str, secs: Option<i64>, took_ms: Option<i64>, started: i64 },
    Tool { id: &'a str, name: &'a str, title: &'a str, status: &'a ToolStatus, files: &'a [Attachment], input: &'a serde_json::Value, metadata: &'a serde_json::Value },
    /// A notice folded into the turn's group.
    Notice(&'a Notice),
    /// Commentary folded into the group: text a later tool call in the turn follows.
    Text { body: &'a str },
    /// A change script `script` in assistant message `msg` made to a file, at offset `index` of
    /// its `metadata.files` (`rules::script_changes`), placed by the Edits setting as a direct
    /// edit's file is (`placeScriptChange`, `scriptChangeItems`).
    Change { msg: &'a str, script: &'a str, index: usize, file: &'a serde_json::Value },
}

/// How calls on their own row draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Calls {
    /// One call as itself.
    One,
    /// Plain reads sharing a line.
    Reads,
    /// Edits, writes and patches sharing a file card.
    Files,
}

/// Where a group's step comes from: its entry (the turn's message, counted from the turn's
/// first) and part, so an open group's virtual rows find a step again from the transcript
/// without folding the turn. `n`: a thought's index among its message's thoughts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    pub entry: u32,
    pub part: u32,
    pub n: u32,
}

impl Loc {
    /// A notice's entry has no part.
    const NOTICE: u32 = u32::MAX;

    /// A tool part's `n` is 0 for the call itself, else 1 past its change's offset in the
    /// script's `metadata.files`.
    ///
    /// The step this names in the turn's `messages` (one entry each, as [`fold_with`] read them).
    pub fn step<'a>(self, messages: &'a [opencode_kit::model::Message]) -> Option<Step<'a>> {
        use opencode_kit::model::Message;
        let m = messages.get(self.entry as usize)?;
        if self.part == Loc::NOTICE {
            return match m {
                Message::Notice(n) => Some(Step::Notice(n)),
                _ => None,
            };
        }
        let a = match m {
            Message::Assistant(a) => a,
            Message::Note { step, .. } => &**step,
            _ => return None,
        };
        Some(match a.parts.get(self.part as usize)? {
            Part::Text { text } => Step::Text { body: text },
            Part::Reasoning { text, started, completed } => Step::Thought {
                msg: &a.id,
                index: self.n as usize,
                body: text,
                secs: completed.map(|c| ((c - started) as f64 / 1000.0).round().max(0.0) as i64),
                took_ms: completed.map(|c| (c - started).max(0)),
                started: *started,
            },
            Part::Tool { id, status, name, metadata, .. } if self.n > 0 => {
                let index = self.n as usize - 1;
                Step::Change { msg: &a.id, script: id, index, file: rules::script_change_at(name, status, metadata, index)? }
            }
            Part::Tool { id, name, title, status, input, files, metadata } => Step::Tool { id, name, title, status, files, input, metadata },
        })
    }
}

/// One timeline row of a turn's assistant work.
#[derive(Debug)]
pub enum Block<'a> {
    /// Folded thoughts, tool calls, notices and commentary; `at`: each step's [`Loc`].
    Group { key: String, steps: Vec<Step<'a>>, at: Vec<Loc> },
    Text { key: String, body: &'a str, msg: usize, part: usize, live: bool },
    /// Background shells started together, or reporting back one after another, as one events
    /// row (`groupContent`'s `events`, `groupNotices`): "Started 2 shells".
    Events { key: String, steps: Vec<Step<'a>> },
    /// One subagent event, a call or a report, on a row of its own (`groupContent`'s
    /// `subagent`, anomalyco/opencode#64017).
    Subagent { key: String, step: Step<'a> },
    /// A question call, once answered, dismissed or failed (`question_row`).
    Question { id: &'a str, status: &'a ToolStatus, input: &'a serde_json::Value, metadata: &'a serde_json::Value },
    /// Thoughts on a row of their own, one after another with nothing between (`groupContent`'s
    /// `thought`): Thinking separately, or (`live`) the row that stands at the end of a running
    /// turn in place of the status line (`TimelineRow.Thinking`): the thought the model is
    /// writing after the finished ones it follows (`before`), or the finished thoughts the turn
    /// ends on. Its key is its first thought's, so a thought joining it keeps the row.
    Thought { key: String, steps: Vec<Step<'a>>, live: bool },
    /// Calls on their own row (their kind shows separately).
    Calls { key: String, kind: Calls, steps: Vec<Step<'a>> },
}

/// Whether every step is a thought, or every step a notice: notices never join a run of
/// thoughts (`projection.ts`'s `joins`).
fn all_thoughts(group: &[Step]) -> bool {
    !group.is_empty() && group.iter().all(|s| matches!(s, Step::Thought { .. }))
}

fn all_notices(group: &[Step]) -> bool {
    !group.is_empty() && group.iter().all(|s| matches!(s, Step::Notice(_)))
}

/// The finished thoughts the rows so far end on, with nothing after them, taken off: the open
/// group's last steps, or the row of thoughts last pushed (`trailingThoughts`,
/// `projection.ts:632-637`).
fn take_tail<'a>(out: &mut Vec<Block<'a>>, group: &mut Vec<Step<'a>>, at: &mut Vec<Loc>, key: &mut Option<String>) -> Vec<Step<'a>> {
    if group.is_empty() {
        if !matches!(out.last(), Some(Block::Thought { live: false, .. })) {
            return Vec::new();
        }
        return match out.pop() {
            Some(Block::Thought { steps, .. }) => steps,
            _ => Vec::new(),
        };
    }
    let from = group.iter().rposition(|s| !matches!(s, Step::Thought { .. })).map_or(0, |i| i + 1);
    at.truncate(from);
    let tail = group.split_off(from);
    if group.is_empty() {
        *key = None;
    }
    tail
}

/// A row of thoughts' key: its first thought's.
fn thought_row_key(steps: &[Step]) -> String {
    match steps.first() {
        Some(Step::Thought { msg, index, .. }) => format!("th-{msg}-{index}"),
        _ => String::from("th-"),
    }
}

/// Folds consecutive assistant messages (a turn's steps) as Desktop's timeline does, by the
/// default Timeline detail. `live`: the last one is still streaming.
pub fn fold<'a>(steps: &[&'a Assistant], live: bool) -> Vec<Block<'a>> {
    let entries: Vec<Entry<'a>> = steps.iter().map(|a| Entry::Step(a)).collect();
    fold_with(&entries, live, &Detail::default())
}

/// Whether an events block holds only reports (notices), or only launches (calls): the two never
/// share a row (`projection.ts`'s `joins`, `groupContent`).
fn events_of_notices(steps: &[Step]) -> bool {
    steps.iter().all(|s| matches!(s, Step::Notice(_)))
}

/// Whether the open activity group started the work `e` is about (`launches`).
fn launched(group: &[Step], e: &events::Event) -> bool {
    let Some(key) = events::work_key(e) else { return false };
    group.iter().any(|st| matches!(st, Step::Tool { .. }) && crate::activity::label::step_event(st).is_some_and(|l| events::work_key(&l) == Some(key)))
}

/// A turn's `entries` as rows, by `detail`. `live`: the last step is still streaming. A block's
/// `msg` is its entry's index.
pub fn fold_with<'a>(entries: &[Entry<'a>], live: bool, detail: &Detail) -> Vec<Block<'a>> {
    fold_with_flags(entries, live, detail, false)
}

/// Like [`fold_with`], gated by `script_diffs` (the server's `full_codemode` flag,
/// `packages/session-ui/src/timeline/facts.ts:126,223,283`, `opencode#66509`): without it, a
/// script's file changes neither show nor count.
pub fn fold_with_flags<'a>(entries: &[Entry<'a>], live: bool, detail: &Detail, script_diffs: bool) -> Vec<Block<'a>> {
    let commentary = commentary(entries, !live);
    let mut out: Vec<Block<'a>> = Vec::new();
    let mut group: Vec<Step<'a>> = Vec::new();
    let mut at: Vec<Loc> = Vec::new();
    let mut key: Option<String> = None;
    let flush = |out: &mut Vec<Block<'a>>, group: &mut Vec<Step<'a>>, at: &mut Vec<Loc>, key: &mut Option<String>| {
        if !group.is_empty() {
            let key = key.take().unwrap_or_else(|| String::from("c-"));
            out.push(Block::Group { key, steps: std::mem::take(group), at: std::mem::take(at) });
        }
    };
    // An events block joins the one before it only while nothing came between.
    let push_event = |out: &mut Vec<Block<'a>>, key: String, step: Step<'a>| {
        let notice = matches!(step, Step::Notice(_));
        match out.last_mut() {
            Some(Block::Events { steps, .. }) if events_of_notices(steps) == notice => steps.push(step),
            _ => out.push(Block::Events { key, steps: vec![step] }),
        }
    };
    let last = entries.iter().rposition(|e| matches!(e, Entry::Step(_)));
    for (mi, e) in entries.iter().enumerate() {
        let a = match e {
            Entry::Notice(n) => {
                // Background work reporting back (`groupNotices`): a subagent's report is a row of
                // its own; a shell's, right after the group that started it, finishes there, else
                // it is an event of its own.
                if let Some(ev) = events::notice_event(n) {
                    if ev.kind == events::Kind::Subagent {
                        flush(&mut out, &mut group, &mut at, &mut key);
                        out.push(Block::Subagent { key: format!("a-{}", n.id), step: Step::Notice(n) });
                    } else if !ev.failed && launched(&group, &ev) {
                        group.push(Step::Notice(n));
                        at.push(Loc { entry: mi as u32, part: Loc::NOTICE, n: 0 });
                    } else {
                        flush(&mut out, &mut group, &mut at, &mut key);
                        push_event(&mut out, format!("c-ev-{}", n.id), Step::Notice(n));
                    }
                    continue;
                }
                if all_thoughts(&group) {
                    flush(&mut out, &mut group, &mut at, &mut key);
                }
                if group.is_empty() {
                    key = Some(format!("c-{}", n.id));
                }
                group.push(Step::Notice(n));
                at.push(Loc { entry: mi as u32, part: Loc::NOTICE, n: 0 });
                continue;
            }
            Entry::Skip | Entry::Idle => continue,
            Entry::Step(a) => *a,
        };
        let live_msg = live && Some(mi) == last;
        // Keys count each kind apart, as the server's ordinals do: a thought arriving or
        // showing up in a refetched copy doesn't re-key the answer beside it.
        let (mut texts, mut thoughts) = (0, 0);
        for (pi, part) in a.parts.iter().enumerate() {
            let is_last = live_msg && pi + 1 == a.parts.len();
            // The group's key is its first step's: only that one formats it.
            let fold_in = |group: &mut Vec<Step<'a>>, at: &mut Vec<Loc>, key: &mut Option<String>, first: &dyn Fn() -> String, step: Step<'a>, n: usize| {
                if group.is_empty() {
                    *key = Some(first());
                }
                group.push(step);
                at.push(Loc { entry: mi as u32, part: pi as u32, n: n as u32 });
            };
            match part {
                Part::Text { text } => {
                    let n = texts;
                    texts += 1;
                    // A live, still-empty answer keeps its row so the stream reveals into it.
                    if text.trim().is_empty() && !is_last {
                        continue;
                    }
                    let placement = if commentary.binary_search(&(mi, pi)).is_ok() { detail.placement(Category::Commentary) } else { Placement::Separate };
                    match placement {
                        Placement::Hidden => {}
                        Placement::Grouped => fold_in(&mut group, &mut at, &mut key, &|| format!("c-{}-t{n}", a.id), Step::Text { body: text }, 0),
                        Placement::Separate => {
                            flush(&mut out, &mut group, &mut at, &mut key);
                            out.push(Block::Text { key: format!("t-{}-{n}", a.id), body: text, msg: mi, part: pi, live: is_last });
                        }
                    }
                }
                Part::Reasoning { text, started, completed } => {
                    let n = thoughts;
                    thoughts += 1;
                    if detail.placement(Category::Thinking) == Placement::Hidden {
                        continue;
                    }
                    let secs = completed.map(|c| ((c - started) as f64 / 1000.0).round().max(0.0) as i64);
                    let step = Step::Thought { msg: &a.id, index: n, body: text, secs, took_ms: completed.map(|c| (c - started).max(0)), started: *started };
                    // The thought the model is writing stands at the end of the turn, even before
                    // its first words (`textShows`, `stepEvent`'s `reasoning`), after the finished
                    // thoughts it follows with nothing between (`projection.ts:568-575`, `before`).
                    if is_last && completed.is_none() {
                        let mut steps = take_tail(&mut out, &mut group, &mut at, &mut key);
                        flush(&mut out, &mut group, &mut at, &mut key);
                        steps.push(step);
                        out.push(Block::Thought { key: thought_row_key(&steps), steps, live: true });
                        continue;
                    }
                    if text.trim().is_empty() {
                        continue;
                    }
                    match detail.placement(Category::Thinking) {
                        Placement::Grouped => {
                            if all_notices(&group) {
                                flush(&mut out, &mut group, &mut at, &mut key);
                            }
                            fold_in(&mut group, &mut at, &mut key, &|| format!("c-{}-{n}", a.id), step, n)
                        }
                        // A thought right after a row of thoughts joins it (`projection.ts:956-988`).
                        _ if group.is_empty() && matches!(out.last(), Some(Block::Thought { live: false, .. })) => {
                            if let Some(Block::Thought { steps, .. }) = out.last_mut() {
                                steps.push(step);
                            }
                        }
                        _ => {
                            flush(&mut out, &mut group, &mut at, &mut key);
                            out.push(Block::Thought { key: format!("th-{}-{n}", a.id), steps: vec![step], live: false });
                        }
                    }
                }
                Part::Tool { id, name, title, status, input, files, metadata } => {
                    let step = Step::Tool { id, name, title, status, files, input, metadata };
                    let rule = rules::rule(name, input);
                    // A call stopped before the model finished writing it did nothing.
                    let empty = input.as_object().is_none_or(|o| o.is_empty());
                    if rule.hidden {
                        continue;
                    }
                    // Its changes still show under `full_codemode`: they happened (`scriptChangeItems` reads every part).
                    if rules::stopped(status, metadata) && empty {
                        if script_diffs {
                            place_changes(&mut out, &mut group, &mut at, &mut key, detail, (mi, pi), (&a.id, id, name, status, metadata));
                        }
                        continue;
                    }
                    let failed = rules::failed(name, status, metadata);
                    if rule.alone {
                        if !matches!(status, ToolStatus::Pending | ToolStatus::Running) {
                            flush(&mut out, &mut group, &mut at, &mut key);
                            out.push(Block::Question { id, status, input, metadata });
                        }
                        continue;
                    }
                    let placement = detail.placement(rule.category);
                    // Every subagent event is its own row, wherever its setting places it, unless
                    // hidden: they are never counted (`contentGroup`'s `subagent`).
                    if rule.category == Category::Subagents {
                        if placement != Placement::Hidden {
                            flush(&mut out, &mut group, &mut at, &mut key);
                            out.push(Block::Subagent { key: format!("a-{id}"), step });
                        }
                        continue;
                    }
                    match placement {
                        // A failure shows only where its category does (`contentShows`).
                        Placement::Hidden => {}
                        // Work that runs on joins the activity group where its setting groups
                        // it; a launch that failed keeps its own row, as failures do.
                        Placement::Grouped if rule.layout != rules::Layout::Event || !failed => fold_in(&mut group, &mut at, &mut key, &|| format!("c-{id}"), step, 0),
                        _ if rule.layout == rules::Layout::Event => {
                            flush(&mut out, &mut group, &mut at, &mut key);
                            push_event(&mut out, format!("c-ev-{id}"), step);
                        }
                        _ => {
                            flush(&mut out, &mut group, &mut at, &mut key);
                            let kind = if grouped_read(&step) {
                                Calls::Reads
                            } else if !failed && file_tool(&step) {
                                Calls::Files
                            } else {
                                Calls::One
                            };
                            match out.last_mut() {
                                Some(Block::Calls { kind: k, steps, .. }) if *k == kind && kind != Calls::One => steps.push(step),
                                _ => out.push(Block::Calls { key: format!("p-{id}"), kind, steps: vec![step] }),
                            }
                        }
                    }
                    if script_diffs {
                        place_changes(&mut out, &mut group, &mut at, &mut key, detail, (mi, pi), (&a.id, id, name, status, metadata));
                    }
                }
            }
        }
    }
    // While the turn runs, the thoughts it ends on read as Thinking until something else shows,
    // since the model may think again in its next step (`projection.ts:499-503`, `:525-530`).
    let tail = if live { take_tail(&mut out, &mut group, &mut at, &mut key) } else { Vec::new() };
    flush(&mut out, &mut group, &mut at, &mut key);
    if !tail.is_empty() {
        out.push(Block::Thought { key: thought_row_key(&tail), steps: tail, live: true });
    }
    out
}

/// A script's changes, right after it, placed by the Edits setting even where the script itself
/// hides (`scriptChangeItems`, `placeScriptChange`): in the group, beside the files edited around
/// them, or not at all. `(mi, pi)`: the script's entry and part.
#[allow(clippy::type_complexity)]
fn place_changes<'a>(
    out: &mut Vec<Block<'a>>,
    group: &mut Vec<Step<'a>>,
    at: &mut Vec<Loc>,
    key: &mut Option<String>,
    detail: &Detail,
    (mi, pi): (usize, usize),
    (msg, id, name, status, metadata): (&'a str, &'a str, &str, &ToolStatus, &'a serde_json::Value),
) {
    let placement = detail.placement(Category::Edit);
    if placement == Placement::Hidden {
        return;
    }
    for (k, file) in rules::script_changes(name, status, metadata) {
        let step = Step::Change { msg, script: id, index: k, file };
        if placement == Placement::Grouped {
            if group.is_empty() {
                *key = Some(format!("c-{msg}-{id}-f{k}"));
            }
            group.push(step);
            at.push(Loc { entry: mi as u32, part: pi as u32, n: k as u32 + 1 });
            continue;
        }
        if !group.is_empty() {
            let key = key.take().unwrap_or_else(|| String::from("c-"));
            out.push(Block::Group { key, steps: std::mem::take(group), at: std::mem::take(at) });
        }
        match out.last_mut() {
            Some(Block::Calls { kind: Calls::Files, steps, .. }) => steps.push(step),
            _ => out.push(Block::Calls { key: format!("p-{msg}-{id}-f{k}"), kind: Calls::Files, steps: vec![step] }),
        }
    }
}

/// The turn's commentary (`turnCommentary`, `session-ui/src/timeline/commentary.ts`,
/// anomalyco/opencode#64917), by entry and part, run by run: a prompt starts a turn, but wake-ups
/// from watches, subagents and jobs add idle-bounded runs to it, and each run keeps its own answer
/// out of the activity group. A run an idle marker closed is settled; the turn's last run is
/// settled once the turn is (`settled`). Sorted, for `binary_search`.
fn commentary(entries: &[Entry], settled: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    for (i, e) in entries.iter().enumerate() {
        if matches!(e, Entry::Idle) {
            run_commentary(entries, from..i, true, &mut out);
            from = i + 1;
        }
    }
    run_commentary(entries, from..entries.len(), settled, &mut out);
    out
}

/// One run's commentary (`runCommentary`): provider `native.phase` labels take precedence over
/// tool order (`commentary` always groups, `final_answer` never groups); otherwise text before
/// the run's last tool call is commentary. A `settled` run whose every visible text would be
/// commentary keeps its last as the answer, so an interrupted or tool-ending run still leaves
/// something to read.
fn run_commentary(entries: &[Entry], run: std::ops::Range<usize>, settled: bool, out: &mut Vec<(usize, usize)>) {
    use opencode_kit::model::TextPhase;
    let parts = || entries[run.clone()].iter().zip(run.clone()).flat_map(|(e, mi)| {
        let step = match e {
            Entry::Step(a) => Some(*a),
            Entry::Notice(_) | Entry::Skip | Entry::Idle => None,
        };
        step.into_iter().flat_map(move |a| a.parts.iter().enumerate().map(move |(pi, p)| ((mi, pi), p, a.text_phase(pi))))
    });
    let last_tool = parts().rfind(|(_, p, _)| matches!(p, Part::Tool { .. })).map(|(at, _, _)| at);
    let start = out.len();
    for (at, p, phase) in parts() {
        if !matches!(p, Part::Text { .. }) {
            continue;
        }
        let is_commentary = phase == Some(TextPhase::Commentary)
            || (phase != Some(TextPhase::FinalAnswer) && last_tool.is_some_and(|lt| at < lt));
        if is_commentary {
            out.push(at);
        }
    }
    if start == out.len() {
        return;
    }
    let slice = &out[start..];
    let has_answer = parts().any(|(at, p, _)| {
        matches!(p, Part::Text { text } if !text.trim().is_empty()) && slice.binary_search(&at).is_err()
    });
    if settled && !has_answer {
        let last_visible = parts().rfind(|(_, p, _)| matches!(p, Part::Text { text } if !text.trim().is_empty())).map(|(at, _, _)| at);
        if let Some(answer) = last_visible
            && let Some(i) = out[start..].iter().position(|t| *t == answer)
        {
            out.remove(start + i);
        }
    }
}

/// A tool's name in a group's label (`toolTitle`).
pub fn tool_title(name: &str) -> &str {
    use opencode_i18n::t;
    match name {
        "read" => t!("ui.tool.read"),
        "list" => t!("ui.tool.list"),
        "glob" => t!("ui.tool.glob"),
        "grep" => t!("ui.tool.grep"),
        "browser" => t!("ui.tool.browser"),
        "webfetch" => t!("ui.tool.webfetch"),
        "websearch" => t!("ui.tool.websearch"),
        "subagent" | "task" => t!("ui.tool.agent.default"),
        "shell" | "bash" => t!("ui.tool.shell"),
        "execute" => t!("ui.tool.execute"),
        "edit" => t!("ui.messagePart.title.edit"),
        "write" => t!("ui.messagePart.title.write"),
        "patch" | "apply_patch" => t!("ui.tool.patch"),
        "question" => t!("ui.tool.questions"),
        "skill" => t!("ui.tool.skill"),
        _ => name,
    }
}

/// The label of a group with no calls and no work that runs on (`usedLabel`):
/// `ui.messagePart.tools.used` ("Used {count} {title}") over its thoughts, split around its
/// count and title as Desktop splits it, or "Updates" alone for notices and commentary (in
/// `before`).
#[derive(Debug, PartialEq)]
pub struct Label {
    pub count: Option<usize>,
    pub title: String,
    /// The words before the count, between it and the title, and after the title.
    pub before: String,
    pub between: String,
    pub after: String,
    /// The whole label, as its button names it.
    pub text: String,
}

impl Label {
    pub fn text(&self) -> String {
        self.text.clone()
    }

    /// `ui.messagePart.tools.used` for `count` and `title`, cut where its count and title stand.
    fn used(count: usize, title: String) -> Label {
        let text = opencode_i18n::plural!("ui.messagePart.tools.used", count, tools = title);
        let at = text.find(&title).unwrap_or(text.len());
        let before = text[..at].trim();
        let after = text.get(at + title.len()..).unwrap_or("").trim().to_owned();
        let digits = count.to_string();
        match before.find(&digits) {
            Some(n) => Label { count: Some(count), before: before[..n].trim().to_owned(), between: before[n + digits.len()..].trim().to_owned(), after, title, text },
            None => Label { count: None, before: before.to_owned(), between: String::new(), after, title, text },
        }
    }
}

/// "Used 2 Thoughts", or "Updates" (`usedLabel`). Thoughts one after another count as one, as
/// they show (`activity-label.ts:23-25`).
pub fn label(steps: &[Step]) -> Label {
    let is_thought = |s: &Step| matches!(s, Step::Thought { .. });
    let thoughts = steps.iter().enumerate().filter(|(i, s)| is_thought(s) && !(*i > 0 && is_thought(&steps[i - 1]))).count();
    if thoughts == 0 {
        let updates = opencode_i18n::t!("ui.messagePart.context.updates");
        return Label { count: None, title: String::new(), before: updates.into(), between: String::new(), after: String::new(), text: updates.into() };
    }
    Label::used(thoughts, opencode_i18n::plural!("ui.messagePart.context.thought", thoughts))
}

/// How long something took, as Desktop counts it (`ui.message.duration.*`): "3s",
/// "1m 5s", "1h 2m".
pub fn elapsed(ms: i64) -> String {
    use opencode_i18n::{integer, t};
    let total = (ms / 1000).max(0);
    if total < 60 {
        return t!("ui.message.duration.seconds", count = integer(total));
    }
    if total < 3600 {
        return t!("ui.message.duration.minutesSeconds", minutes = integer(total / 60), seconds = integer(total % 60));
    }
    t!("ui.message.duration.hoursMinutes", hours = integer(total / 3600), minutes = integer(total / 60 % 60))
}

/// Step `st` as a tool call, when it is one.
fn call<'a>(st: &Step<'a>) -> Option<crate::tools::Call<'a>> {
    match st {
        Step::Tool { id, name, status, input, metadata, .. } => Some(crate::tools::Call { id, name, status, input, metadata }),
        Step::Thought { .. } | Step::Notice(_) | Step::Text { .. } | Step::Change { .. } => None,
    }
}

/// A read that shares a line with the reads beside it: not failed, not an image, and loading no
/// instructions (`currentToolGroupedRead`).
fn grouped_read(st: &Step) -> bool {
    let Step::Tool { name: "read", status, input, files, metadata, .. } = st else { return false };
    let image = crate::images::shows(files) || crate::tools::path(input).is_some_and(|p| {
        let p = p.to_ascii_lowercase();
        [".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".avif", ".bmp", ".ico"].iter().any(|e| p.ends_with(e))
    });
    let loaded = metadata["loaded"].as_array().is_some_and(|a| a.iter().any(|v| v.is_string()));
    **status != ToolStatus::Error && !image && !loaded
}


/// An edit, write or patch that didn't fail: it joins the file card of the steps beside it.
fn file_tool(st: &Step) -> bool {
    call(st).is_some_and(|c| crate::tools::is_file_tool(&c))
}

/// A change a script made, which shows with the files edited beside it.
fn script_change(st: &Step) -> bool {
    matches!(st, Step::Change { .. })
}

/// A file edit or a script's change: they share a row, in the order they happened (`fileRuns`).
fn file_like(st: &Step) -> bool {
    file_tool(st) || script_change(st)
}

/// A skill call with a name that didn't fail: it joins the "Loaded" line of the skills beside it.
fn named_skill(st: &Step) -> bool {
    matches!(st, Step::Tool { name: "skill", status, .. } if **status != ToolStatus::Error) && call(st).is_some_and(|c| crate::tools::skill_name(&c).is_some())
}

/// Computes where each `execute` script's Output row goes inside a slice of consecutive steps
/// (`packages/session-ui/src/timeline/script-output.ts`, `opencode#66509`).
pub fn script_output_for_steps<'a>(steps: &[Step<'a>], flat: bool) -> crate::tools::script::OutputPlacement<'a> {
    let mut out = crate::tools::script::OutputPlacement::default();
    if !flat {
        return out;
    }
    for (i, st) in steps.iter().enumerate() {
        let Step::Tool { id, name: "execute", status, input, metadata, .. } = st else { continue };
        let mut last: Option<(&str, &str, usize)> = None;
        for next in &steps[i + 1..] {
            match next {
                Step::Change { msg, script, index, .. } if *script == *id => last = Some((*msg, *script, *index)),
                _ => break,
            }
        }
        if let Some((msg, script, index)) = last {
            let call = crate::tools::Call { id, name: "execute", status, input, metadata };
            out.after_changes.insert(*id);
            out.output_after.insert(crate::tools::script::change_id(msg, script, index), (*id, crate::tools::script_output_text(&call)));
        }
    }
    out
}

/// Computes cross-block Output placement across a turn's top-level `blocks`
/// (`packages/session-ui/src/timeline/script-output.ts:57-76`, `opencode#66509`): when a block
/// ends on an `execute` call and the very next block is `Calls::Files` beginning with that
/// script's changes, the script's Output row moves after its last consecutive change in that block.
pub fn script_output_for_blocks<'a>(blocks: &[Block<'a>], flat: bool) -> crate::tools::script::OutputPlacement<'a> {
    let mut out = crate::tools::script::OutputPlacement::default();
    if !flat {
        return out;
    }
    for (i, b) in blocks.iter().enumerate() {
        let last_step = match b {
            Block::Group { steps, .. } | Block::Calls { steps, .. } => steps.last(),
            _ => None,
        };
        let Some(Step::Tool { id, name: "execute", status, input, metadata, .. }) = last_step else { continue };
        let Some(Block::Calls { kind: Calls::Files, steps: next_steps, .. }) = blocks.get(i + 1) else { continue };
        let mut last: Option<(&str, &str, usize)> = None;
        for next in next_steps {
            match next {
                Step::Change { msg, script, index, .. } if *script == *id => last = Some((*msg, *script, *index)),
                _ => break,
            }
        }
        if let Some((msg, script, index)) = last {
            let call = crate::tools::Call { id, name: "execute", status, input, metadata };
            out.after_changes.insert(*id);
            out.output_after.insert(crate::tools::script::change_id(msg, script, index), (*id, crate::tools::script_output_text(&call)));
        }
    }
    out
}
