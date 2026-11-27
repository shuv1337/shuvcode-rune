//! A group's label, named by what the agent did (`context-group.tsx`'s `activityLabel` and
//! `workEvents`): its main mode, then what its other calls did. "Edited 2 files · 3 reads,
//! 1 command", "Explored 4 files, 2 searches", "Started 1 shell · finished 1 shell". Subagents
//! never count: each is a row of its own (anomalyco/opencode#64017). Every count comes from the
//! steps' parts, never from the rows drawn (`timeline/facts.ts`' `summarize`, #65012).

use std::collections::{HashMap, HashSet};

use super::events::{self, Event, Kind, Verb};
use super::rules::{self, Layout, Mode};
use crate::turns::Step;

/// The label's words: the verb in muted ink, what it touched in base ink, and the rest after
/// a dot.
#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    pub verb: String,
    pub main: String,
    pub others: Vec<String>,
}

impl Activity {
    /// The whole label, as its button names it.
    pub fn text(&self) -> String {
        let head = [self.verb.as_str(), self.main.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
        if self.others.is_empty() { head } else { format!("{head} · {}", self.others.join(", ")) }
    }
}

/// The event a step is, where it is one: a subagent or background shell call, or a notice of
/// one reporting back.
pub fn step_event(st: &Step) -> Option<Event> {
    match st {
        Step::Tool { name, title, status, input, metadata, .. } if rules::rule(name, input).layout == Layout::Event => events::tool_event(name, title, status, input, metadata),
        Step::Notice(n) => events::notice_event(n),
        _ => None,
    }
}

/// What each background shell in a group did (`workEvents`): one that reports back in the group
/// that started it counts once, as how it ended.
pub fn work_events(steps: &[Step]) -> Vec<Event> {
    let launch = |st: &Step| match st {
        Step::Tool { .. } => step_event(st),
        _ => None,
    };
    let launched: HashSet<(Kind, String)> = steps.iter().filter_map(launch).filter_map(|e| events::work_key(&e).map(|(k, id)| (k, id.to_owned()))).collect();
    let reports: HashMap<(Kind, String), Event> = steps.iter().filter(|st| matches!(st, Step::Notice(_))).filter_map(step_event)
        .filter_map(|e| events::work_key(&e).map(|(k, id)| ((k, id.to_owned()), e.clone()))).collect();
    steps.iter().filter_map(|st| match st {
        Step::Notice(_) => step_event(st).filter(|e| events::work_key(e).is_none_or(|(k, id)| !launched.contains(&(k, id.to_owned())))),
        Step::Tool { .. } => {
            let e = launch(st)?;
            let outcome = events::work_key(&e).and_then(|(k, id)| reports.get(&(k, id.to_owned())));
            Some(match outcome {
                Some(o) if e.verb == Verb::Started => Event { verb: o.verb, failed: o.failed, ..e },
                _ => e,
            })
        }
        _ => None,
    }).collect()
}

/// The most consequential work names a group: changing files over running commands, and both
/// over reading (`modePriority`).
const PRIORITY: [Mode; 4] = [Mode::Edit, Mode::Run, Mode::Tool, Mode::Explore];

/// One counted call inside a group: either a direct tool call or, under `full_codemode`,
/// a nested step inside an `execute` script (`stepFacts`, `facts.ts:315-350`, `opencode#66650`).
struct Counted<'s> {
    raw_name: &'s str,
    input: &'s serde_json::Value,
    metadata: &'s serde_json::Value,
    rule: rules::Rule,
    from_script: bool,
}

/// Expands an `execute` script's `metadata.toolCalls` into direct-equivalent calls
/// (`stepFacts`, `packages/session-ui/src/timeline/facts.ts:307-350`, `opencode#66650`):
/// `Bun.write` -> `edit`, `Bun.file` -> `read`, other `Bun` calls count nothing, and
/// Code Mode's shell has no background.
fn script_step_calls<'s>(metadata: &'s serde_json::Value) -> Vec<Counted<'s>> {
    static EMPTY: serde_json::Value = serde_json::Value::Null;
    let Some(calls) = metadata["toolCalls"].as_array() else { return Vec::new() };
    let mut out = Vec::new();
    for call in calls {
        let Some(tool) = call["tool"].as_str() else { continue };
        let Some(status) = call["status"].as_str() else { continue };
        if !matches!(status, "running" | "completed" | "error") {
            continue;
        }
        let input = call.get("input").filter(|v| v.is_object()).unwrap_or(&EMPTY);
        let mapped = if tool == "Bun" {
            match input["method"].as_str() {
                Some("write") => "edit",
                Some("file") => "read",
                _ => continue,
            }
        } else {
            tool
        };
        // Code Mode's shell has no background (`facts.ts:343`), so evaluate rules with empty input.
        let rule = rules::rule(mapped, &EMPTY);
        out.push(Counted { raw_name: mapped, input, metadata: &EMPTY, rule, from_script: true });
    }
    out
}

/// A group's activity label, or `None` for one with neither calls nor work that runs on.
pub fn activity<'s>(steps: &[Step<'s>]) -> Option<Activity> {
    activity_with(steps, false)
}

/// Like [`activity`]; with `script_steps` (`full_codemode`, `opencode#66650`), a script counts
/// as the calls it made rather than as itself, and an edit step leads only once its files have
/// landed (`activity-label.ts:53-59`).
pub fn activity_with<'s>(steps: &[Step<'s>], script_steps: bool) -> Option<Activity> {
    use opencode_i18n::{plural, t};
    let mut tools: Vec<Counted<'s>> = Vec::new();
    for st in steps {
        let Step::Tool { name, input, metadata, .. } = st else { continue };
        let rule = rules::rule(name, input);
        if rule.layout == Layout::Event {
            continue;
        }
        if script_steps && rule.name == "execute" {
            let expanded = script_step_calls(metadata);
            if !expanded.is_empty() {
                tools.extend(expanded);
                continue;
            }
        }
        tools.push(Counted { raw_name: name, input, metadata, rule, from_script: false });
    }
    let (buckets, _) = events::summarize_shells(&work_events(steps));
    let launched: Vec<String> = buckets.iter().map(|b| events::launch_count(b.verb, b.count)).collect();
    // Each path once, in the order first seen.
    let mut files: Vec<&'s str> = Vec::new();
    let mut seen: HashSet<&'s str> = HashSet::new();
    let mut add = |p: &'s str| {
        if seen.insert(p) {
            files.push(p);
        }
    };
    for t in tools.iter().filter(|t| !t.from_script && t.rule.mode == Mode::Edit) {
        let changed: Vec<&'s str> = t.metadata["files"].as_array().into_iter().flatten().filter_map(|f| f["file"].as_str()).collect();
        let paths = if changed.is_empty() { crate::tools::path(t.input).into_iter().collect() } else { changed };
        paths.into_iter().for_each(&mut add);
    }
    // The files a script changed count as files where they are placed in the group, after direct
    // edits (`activityFacts`), and name the group as edits do.
    for st in steps {
        if let &Step::Change { file, .. } = st
            && let Some(p) = file["file"].as_str()
        {
            add(p);
        }
    }
    // Counting a script's steps, an edit step leads only once its files have landed
    // (`packages/session-ui/src/timeline-next/tools/activity-label.ts:53-59`).
    let Some(mode) = PRIORITY.into_iter().find(|m| {
        if *m == Mode::Edit && script_steps {
            !files.is_empty()
        } else {
            tools.iter().any(|t| t.rule.mode == *m) || (*m == Mode::Edit && !files.is_empty())
        }
    }) else {
        let first = buckets.first()?;
        return Some(Activity {
            verb: events::verb_word(first.verb).to_owned(),
            main: opencode_i18n::plural!("ui.timeline.event.shells", first.count),
            others: launched[1..].to_vec(),
        });
    };
    let of = |m: Mode| tools.iter().filter(move |t| t.rule.mode == m);
    let count = |names: &[&str]| tools.iter().filter(|t| names.contains(&t.rule.name)).count();
    let mut reads: Vec<&str> = Vec::new();
    for t in tools.iter().filter(|t| t.rule.name == "read") {
        if let Some(p) = crate::tools::path(t.input)
            && !reads.contains(&p)
        {
            reads.push(p);
        }
    }
    // Reads count as files where exploring names the group, and as reads beside other work.
    let explore = |main: bool| -> Vec<String> {
        let mut out = Vec::new();
        let mut add = |n: usize, s: String| if n > 0 { out.push(s) };
        let n = reads.len();
        add(n, if main { plural!("ui.timeline.activity.files", n) } else { plural!("ui.timeline.activity.reads", n) });
        let n = count(&["glob", "grep", "list"]);
        add(n, plural!("ui.timeline.activity.searches", n));
        let n = count(&["websearch"]);
        add(n, plural!("ui.timeline.activity.webSearches", n));
        let n = count(&["webfetch"]);
        add(n, plural!("ui.timeline.activity.pages", n));
        let n = count(&["skill"]);
        add(n, plural!("ui.timeline.activity.skills", n));
        out
    };
    // Commands are counted, never quoted: a command can be any length.
    let runs = of(Mode::Run).count();
    let commands: Vec<String> = (runs > 0).then(|| plural!("ui.timeline.activity.commands", runs)).into_iter().collect();
    let mut used: Vec<&str> = Vec::new();
    for t in of(Mode::Tool) {
        let title = crate::turns::tool_title(rules::current_name(t.raw_name));
        if !used.contains(&title) {
            used.push(title);
        }
    }
    let others_of = |m: Mode| -> Vec<String> {
        match m {
            Mode::Explore => explore(false),
            Mode::Edit if !files.is_empty() => vec![plural!("ui.timeline.activity.editedFiles", files.len())],
            Mode::Edit => Vec::new(),
            Mode::Run => commands.clone(),
            Mode::Tool => used.iter().map(|s| (*s).to_owned()).collect(),
        }
    };
    // A few files read better by name; past that, the count says more than a list that truncates.
    let main = match mode {
        Mode::Edit if files.len() <= 2 => files.iter().map(|p| crate::tools::file_name(p)).collect::<Vec<_>>().join(", "),
        Mode::Edit => plural!("ui.timeline.activity.files", files.len()),
        Mode::Run => commands[0].clone(),
        Mode::Tool => used.join(", "),
        Mode::Explore => explore(true).join(", "),
    };
    let verb = match mode {
        Mode::Explore => t!("ui.timeline.activity.explored"),
        Mode::Edit => t!("ui.timeline.activity.edited"),
        Mode::Run => t!("ui.timeline.activity.ran"),
        Mode::Tool => t!("ui.timeline.activity.used"),
    };
    let others = PRIORITY.into_iter().filter(|m| *m != mode).flat_map(others_of).chain(launched).collect();
    Some(Activity { verb: verb.to_owned(), main, others })
}
