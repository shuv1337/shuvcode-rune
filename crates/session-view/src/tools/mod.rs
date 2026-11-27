//! A tool call's row inside a turn's open group, as Desktop draws it
//! (`session-ui/src/tools/tool-renderer.tsx`, `components/basic-tool.tsx`): a one-line trigger
//! (title, subtitle, `key=value` args), and under it, once pressed open, what the call did:
//! a shell's console, an edit's diff, a search's results, a failure's message.
//!
//! Each call shows in one of five phases, collapsed or expanded ([`Display`]). A call opens
//! while it runs only where Desktop lets it (`allowOpenWhilePending`: shell and execute), and
//! only when it has something to show. What it shows folds open and shut on the group's fold
//! spring (`disclosure::reveal`), the rows below sliding with it.
//! The host may add a button beside a row that opens what the call touched in its side panel
//! ([`TimelineHost::tool_row_action`]).
//!
//! A row builds again only when its call, its open parts or its look change ([`memo`]): the
//! console's text, a diff's lines and their colours are worked out once per change, not once
//! per commit.

mod console;
mod details;
mod diff;
pub mod error;
mod execute;
mod files;
mod lists;
mod log;
pub mod script;
pub(crate) mod plain;

use std::borrow::Cow;
use std::hash::{Hash, Hasher};

use opencode_kit::model::{self, ToolStatus};
use rune::{El, col, el, row, text};
use serde_json::Value;

use crate::TimelineHost;
use crate::focus::FocusExt;
use crate::palette::Pal;
use crate::style::{HOVER_FADE, TOOL_CODE, UI, arrow, readable};

pub use console::{console_open, follow as shell_follow};
pub use details::overlay as details_overlay;
pub use files::{draws_card as file_draws_card, group as file_group, is_file_tool, shut_group};
pub use execute::script_output_text;
pub use lists::{read_group, skill_group, skill_name};
pub use plain::strip_ansi;

pub(crate) fn preview_words<S: TimelineHost>(p:&Pal, head:&str)->El {log::visible::<S>(p,head,TOOL_CODE,p.muted,true,20.0)}

pub(crate) fn raw_output<S: TimelineHost>(s: &S, session: &str, id: &str) -> Option<String> {
    console::with_call(s, session, id, |c| console::current_output(s, &c).to_owned())
}

/// Unchanged lines around each change in a diff of two texts: jsdiff's default, which Desktop's
/// text diffs keep (`createTwoFilesPatch`, Pierre's `parseDiffFromFile`), where `git diff` keeps
/// three.
pub const TEXT_CONTEXT: u32 = 4;

/// A tool call, as its part has it.
#[derive(Clone, Copy)]
pub struct Call<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub status: &'a ToolStatus,
    pub input: &'a Value,
    pub metadata: &'a Value,
}

/// What the rows of one group share.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    /// The session the calls are in: their paths are relative to its directory.
    pub session: &'a str,
    /// The turn is still running. A call a finished or stopped turn left pending or running is
    /// [`Phase::Stopped`]: it never shimmers, so an idle window draws nothing.
    pub live: bool,
    /// How far below the timeline's top a header in these calls sticks, as Desktop's
    /// `--sticky-accordion-top` says: under an open group's header, else at the top.
    pub sticky: f32,
}

/// Where a call is in its run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The model is still writing the call.
    Pending,
    Running,
    /// Pending or running when its turn ended: it never finished.
    Stopped,
    Completed,
    Errored,
}

/// Whether a call shows what it did under its trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fold {
    Collapsed,
    Expanded,
}

/// How a call's row draws: its phase, and whether it is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Display {
    pub phase: Phase,
    pub fold: Fold,
}

impl Display {
    /// Call `c`'s display: open when it was opened, or by default (a shell, where Timeline
    /// detail expands shells), and `opens` says it may be.
    pub fn of<S: TimelineHost>(s: &S, cx: Ctx, c: &Call, opens: bool) -> Self {
        let phase = match c.status {
            ToolStatus::Pending if cx.live => Phase::Pending,
            ToolStatus::Running if cx.live => Phase::Running,
            ToolStatus::Pending | ToolStatus::Running => Phase::Stopped,
            ToolStatus::Completed => Phase::Completed,
            ToolStatus::Error => Phase::Errored,
        };
        let fold = if opens && crate::disclosure::is_open(s, &key(c.id), default_open(s, c)) { Fold::Expanded } else { Fold::Collapsed };
        Display { phase, fold }
    }

    /// Still being written or running: its title shimmers.
    pub fn busy(self) -> bool {
        matches!(self.phase, Phase::Pending | Phase::Running)
    }

    pub fn open(self) -> bool {
        self.fold == Fold::Expanded
    }
}

/// Whether call `c` shows open until pressed (`contentDefaultOpen`): a shell where Timeline
/// detail expands shells, and a call failed where its category is hidden.
pub fn default_open<S: TimelineHost>(s: &S, c: &Call) -> bool {
    let d = s.timeline_detail();
    let category = crate::detail::tool_category(c.name);
    if d.placement(category) == crate::detail::Placement::Hidden && crate::detail::failed(c.name, c.status, c.metadata) {
        return true;
    }
    category == crate::detail::Category::Shell && c.name != "execute" && d.expanded(category)
}

/// Whether `status` is still under way in a turn that is `live`.
pub fn busy(status: &ToolStatus, live: bool) -> bool {
    live && matches!(status, ToolStatus::Pending | ToolStatus::Running)
}

/// The key that holds call `id` open in [`TimelineHost::expanded`].
pub fn key(id: &str) -> String {
    format!("tool:{id}")
}

/// The key that holds part `part` of call `id` open: one file of a patch, a nested call of an
/// execute, its output.
pub fn sub_key(id: &str, part: impl std::fmt::Display) -> String {
    format!("tool:{id}/{part}")
}

/// Whether call `c` shows a body below its 28 pt trigger line (so its visual foot sits at its
/// box's bottom rather than 6 pt inside the trigger).
pub fn shows_body<S: TimelineHost>(s: &S, cx: Ctx, c: &Call) -> bool {
    shows_body_with(s, cx, c, false)
}

/// Like [`shows_body`], with `after_changes` indicating whether a flat `execute` call's Output
/// row moved after its script file changes (`opencode#66509`).
pub fn shows_body_with<S: TimelineHost>(s: &S, cx: Ctx, c: &Call, after_changes: bool) -> bool {
    let cx = if opencode_kit::model::shell_message(c.metadata) { Ctx { live: true, ..cx } } else { cx };
    let stopped_shell = matches!(c.name, "shell" | "bash") && crate::activity::rules::stopped(c.status, c.metadata);
    if *c.status == ToolStatus::Error && c.name != "execute" && !stopped_shell {
        return crate::disclosure::is_open(s, &key(c.id), default_open(s, c));
    }
    match c.name {
        "shell" if opencode_kit::model::shell_message(c.metadata) => {
            let has_err = matches!(c.metadata["status"].as_str(), Some("timeout" | "killed"))
                || (c.metadata["status"].as_str() == Some("exited") && c.metadata["exit"].as_i64().is_some_and(|e| e != 0));
            has_err || matches!(log::Excerpt::new(console::current_output(s, c)), log::Excerpt::Ends { .. }) || Display::of(s, cx, c, true).open()
        }
        "shell" | "bash" => matches!(log::Excerpt::new(console::current_output(s, c)), log::Excerpt::Ends { .. }) || Display::of(s, cx, c, true).open(),
        "execute" => {
            let calls = execute::calls(c.metadata);
            let err = execute::error(c);
            let out = execute::script_output_text(c);
            let flat = s.full_codemode(cx.session);
            let output_here = !out.is_empty() && !(flat && after_changes);
            let opens = !calls.is_empty() || err.is_some() || output_here;
            if flat { opens } else { Display::of(s, cx, c, opens).open() }
        }
        "edit" | "multiedit" | "write" | "patch" | "apply_patch" => files::draws_card(c),
        "websearch" => {
            let out = output(c);
            let opens = !busy(c.status, cx.live) && (out.len() > 8192 || !plain::urls(out).is_empty());
            Display::of(s, cx, c, opens).open()
        }
        _ => false,
    }
}

/// The row of call `c` inside its group, with what it shows open under it, built again only
/// when what it shows changes. A shimmering title isn't cached, so its sweep keeps the run's
/// rhythm.
pub fn view<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, c: &Call) -> El {
    view_with(s, p, cx, c, false)
}

/// Like [`view`]; `after_changes` moves a flat `execute` script's Output row after its file
/// changes (`opencode#66509`).
pub fn view_with<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, c: &Call, after_changes: bool) -> El {
    // A shell command run from the composer runs whether or not its session does: its title
    // shimmers while the command runs (`SessionShellMessage` passes its own status).
    let cx = if opencode_kit::model::shell_message(c.metadata) { Ctx { live: true, ..cx } } else { cx };
    let build = || {
        // A command the user stopped shows as one that ran, its row saying it was stopped
        // (`ToolDisplay`'s `stopped`).
        let stopped_shell = matches!(c.name, "shell" | "bash") && crate::activity::rules::stopped(c.status, c.metadata);
        if *c.status == ToolStatus::Error && c.name != "execute" && !stopped_shell {
            return error::card(s, p, cx.session, c);
        }
        match c.name {
            "shell" if opencode_kit::model::shell_message(c.metadata) => shell_message(s, p, cx, c),
            "shell" | "bash" => console::shell(s, p, cx, c),
            "execute" => execute::execute(s, p, cx, c, after_changes),
            "edit" | "multiedit" | "write" | "patch" | "apply_patch" => files::group(s, p, cx, &[*c]),
            "read" => lists::read(s, p, cx, c),
            "list" | "glob" | "grep" => lists::search(s, p, cx, c),
            "webfetch" => lists::webfetch(s, p, cx, c),
            "websearch" => lists::websearch(s, p, cx, c),
            "skill" => lists::skill_group(s, p, cx, &[*c]),
            _ => lists::generic(s, p, cx, c),
        }
    };
    cached_with(s, p, cx, &[*c], after_changes, build)
}

/// A shell command run from the composer (`SessionShellMessage`): its shell call, and under it
/// the error card of a command that timed out, was cancelled or exited with another code than 0.
fn shell_message<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, c: &Call) -> El {
    // Open, as wide as its console or card rather than the column (`session-shell-message`);
    // shut, its trigger has the column to fade its command out in.
    let call = col().key("shell-message");
    let call = if Display::of(s, cx, c, true).open() { call.self_start() } else { call.w_full() };
    let call = call.child(console::shell(s, p, cx, c));
    let error = match c.metadata["status"].as_str() {
        Some("timeout") => opencode_i18n::t!("ui.tool.shell.timeout").to_string(),
        Some("killed") => opencode_i18n::t!("ui.tool.shell.cancelled").to_string(),
        Some("exited") if c.metadata["exit"].as_i64().is_some_and(|e| e != 0) => {
            opencode_i18n::t!("ui.tool.shell.exit", code = c.metadata["exit"].to_string()).to_string()
        }
        _ => return call,
    };
    // The card is a failed call of its own, with the command as its subtitle.
    let id = format!("{}-error", c.id);
    let metadata = opencode_kit::model::tool_error_metadata(&error);
    let failed = Call { id: &id, name: c.name, status: &ToolStatus::Error, input: c.input, metadata: &metadata };
    // Desktop's card stays shut until pressed (`ToolErrorCard` without `defaultOpen`); the
    // command and its card are as wide as the wider of them.
    let wide = error::shut_width(s, cx.session, &failed).min(lists::column(cx.session));
    call.min_w(wide).child(error::card_with(s, p, cx.session, &failed, false, 24.0))
}

/// `build` for `calls`, kept until one of them, the parts of them held open, their copy
/// buttons, the host's buttons beside them or the look changes.
pub fn cached<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, calls: &[Call], build: impl FnOnce() -> El) -> El {
    cached_with(s, p, cx, calls, false, build)
}

fn cached_with<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, calls: &[Call], after_changes: bool, build: impl FnOnce() -> El) -> El {
    let Some(first) = calls.first() else { return col() };
    if !s.reduced_motion() && calls.iter().any(|c| busy(c.status, cx.live)) {
        return build();
    }
    let mut h = std::hash::DefaultHasher::new();
    (cx.session, cx.live, cx.sticky.to_bits(), crate::scroller::device_scale().to_bits(), p.theme, p.generation, p.dark, opencode_i18n::locale().id(), crate::style::hover_none(), s.reduced_motion(), s.session_directory(cx.session), s.timeline_detail()).hash(&mut h);
    (s.full_codemode(cx.session), after_changes).hash(&mut h);
    let copied = crate::copied::current(s);
    for c in calls {
        (c.id, c.name, std::mem::discriminant(c.status), s.tool_row_action_version(c.id)).hash(&mut h);
        fingerprint(c.input, &mut h);
        fingerprint(c.metadata, &mut h);
        // The output the host read from the call's shell (`console::follow`).
        console::follow(c, |_| false).and_then(|(id, _)| s.shell_output(id)).map(str::len).hash(&mut h);
        // Each part of it the reader touched, and how: one shut where it opens by default reads
        // apart from one never touched.
        let own = key(c.id);
        let mut touched: Vec<(&String, bool)> = s.expanded().touched().filter(|(k, _)| k.starts_with(&own)).collect();
        touched.sort_by(|a, b| a.0.cmp(b.0));
        touched.hash(&mut h);
        copied.iter().filter(|k| k.contains(c.id)).for_each(|k| k.hash(&mut h));
    }
    // The memo keys its root with its own id, so the row keeps its keys one level down.
    rune::memo(format!("tm-{}", first.id), h.finish(), || col().w_full().child(build()))
}

/// A cheap stand-in for `v`'s content: its shape, numbers, and each string's length and ends.
/// Streamed output grows, and a tool's input or result is replaced, never edited in its middle
/// at the same length.
fn fingerprint(v: &Value, h: &mut impl Hasher) {
    match v {
        Value::Null => 0u8.hash(h),
        Value::Bool(b) => (1u8, b).hash(h),
        Value::Number(n) => (2u8, n.as_f64().map(f64::to_bits)).hash(h),
        Value::String(s) => {
            let b = s.as_bytes();
            (3u8, b.len(), &b[..b.len().min(32)], &b[b.len().saturating_sub(32)..]).hash(h);
        }
        Value::Array(a) => {
            (4u8, a.len()).hash(h);
            a.iter().for_each(|x| fingerprint(x, h));
        }
        Value::Object(o) => {
            (5u8, o.len()).hash(h);
            for (k, x) in o {
                k.hash(h);
                fingerprint(x, h);
            }
        }
    }
}

/// A trigger's words: Desktop's `TriggerTitle`.
#[derive(Default)]
pub struct Trigger {
    /// Shimmers while the call runs.
    pub title: Cow<'static, str>,
    /// Desktop's compact tools (shell, execute): a medium title, 6 pt gaps and a 16 pt arrow
    /// box. The rest set their title in the regular weight inside a group, 8 pt from the words
    /// after it.
    pub compact: bool,
    pub subtitle: Option<String>,
    pub args: Vec<String>,
    /// "(3 matches)" after a dot, from a search's count.
    pub matches: Option<String>,
    /// The subtitle in the code font (a shell's command).
    pub code: bool,
    /// A link in the subtitle's place (a fetch's URL).
    pub link: Option<El>,
    /// A tag at the end of the line, and whether it reads as a failure: how a command ended when
    /// it did not succeed (`timeline-next/tools/shell.tsx`'s `shellOutcome`).
    pub tag: Option<(String, bool)>,
}

/// What a trigger does when pressed.
pub enum Press {
    /// Nothing: the call has nothing more to show.
    None,
    /// Opens and closes the call (or part of it) kept open under this key.
    Toggle(String),
}

/// A call's trigger line: the trigger itself, keyed `tool-{id}` (its words, and the arrow when it
/// opens: on hover, or while open), and beside it, as siblings rather than inside the trigger's
/// button, the call's own `action` and the host's side-panel button, shown while the tool is
/// hovered (Desktop's `basic-tool-action`). Wrap it with [`disclosure`] or [`still`], which
/// make the tool the hover group those buttons follow.
// Public tool trigger builder kept stable for callers across apps and features.
#[allow(clippy::too_many_arguments)]
pub fn trigger<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, c: &Call, d: Display, t: Trigger, press: Press, action: Option<El>) -> El {
    let id = format!("tool-{}", c.id);
    let gap = if t.compact { 6.0 } else { 8.0 };
    // A row reads like a group's label (`timeline-next.css`, rows in an opened group): the tool
    // muted, what it worked on bright, counts muted, all at one weight.
    let mut words = row().key("words").gap(gap).center_y().shrink().min_w(0.0).child(title(s, p, &t.title, UI, d));
    if let Some(sub) = t.subtitle.filter(|x| !x.is_empty()) {
        // `basic-tool.css:111` sets `font-variant-numeric: tabular-nums` (inherited under
        // `message-part.css:1438`), which in Inter widens digits and operators (`=`, `/`, `*`).
        let font = if t.code { TOOL_CODE.lh(16.0) } else { UI.features("\"tnum\"") };
        // A command reads left to right in any window; a path, a pattern or a task's words read
        // their own way, and fade at their own end.
        let dir = if t.code { rune::Direction::Ltr } else { rune::Direction::Auto };
        words = words.child(readable(p, text(sub).key("subtitle").dir(dir).font(font).color(p.text).truncate_fade()).shrink().min_w(0.0));
    }
    for (k, a) in t.args.into_iter().enumerate() {
        // `basic-tool.css:157`: `font-variant-numeric: tabular-nums` on tool arguments.
        words = words.child(readable(p, text(a).key(format!("arg-{k}")).dir(rune::Direction::Auto).font(UI.features("\"tnum\"")).color(p.text).truncate_fade()).shrink().min_w(0.0));
    }
    if let Some(l) = t.link {
        words = words.child(l);
    }
    if let Some(m) = t.matches {
        let dot = el().key("dot").size(6.0, 6.0).center().child(el().size(2.25, 2.25).radius(1.125).fill(p.icon_muted));
        words = words.child(dot).child(text(m).key("matches").font(UI).color(p.muted));
    }
    if let Some((label, danger)) = t.tag {
        words = words.child(crate::activity::tag(p, &label, danger));
    }
    let mut button = row().key(id.clone()).h(28.0).gap(if t.compact { 6.0 } else { 0.0 }).center_y().shrink().min_w(0.0).child(words);
    if let Press::Toggle(k) = press {
        let a = el().key("arrow").w(if t.compact { 16.0 } else { 24.0 }).h(24.0).center().child(arrow(p, d.open()));
        button = button
            .child(if d.open() || crate::style::hover_none() { a } else { a.group_opacity(0.0, 1.0) })
            .role("button")
            .label(t.title.into_owned())
            .expanded(d.open())
            .radius(4.0)
            .focus_default(p)
            .hover_group_focus()
            // The arrow fades in under the pointer (`collapsible.css:26-29`).
            .hover_motion(HOVER_FADE, HOVER_FADE)
            .on_press_tx(crate::disclosure::set::<S>(k, d.open(), id));
    }
    let host = s.tool_row_action(p, cx.session, c);
    let actions: Vec<El> = action.into_iter().chain(host).collect();
    // The subtitle a shell or execute drops while open fades out as the arrow and the buttons
    // beside it slide over.
    let line = row().key("line").w_full().h(28.0).gap(2.0).center_y().animate_layout();
    if actions.is_empty() {
        return line.child(button.grow(1.0));
    }
    // The buttons' glyphs rest hidden (`group_opacity`), the buttons themselves still pressable,
    // as Desktop's `basic-tool-action` is.
    line.child(button).child(row().key("actions").gap(2.0).center_y().children(actions))
}

fn title<S: TimelineHost>(s: &S, p: &Pal, t: &str, font: rune::TextStyle, d: Display) -> El {
    if d.busy() {
        return crate::working::shimmer(s, p, t, font, 0.0).key("title");
    }
    text(t.to_owned()).key("title").font(font).color(p.muted)
}

/// A tool with nothing to open: its trigger line, the hover group its buttons follow.
pub fn still(line: El) -> El {
    col().w_full().hover_group_focus().child(line)
}

/// What an open call shows, under its trigger: `rail` insets it 12, as Desktop insets a tool's
/// content; a shell's console and a file's diff run full width.
pub fn body(content: El, rail: bool, gap: f32) -> El {
    let b = col().key("body").w_full().pt(gap).child(content);
    if rail { b.pl(12.0) } else { b }
}

/// A trigger line, and `content` under it while open, folding open and shut; the whole tool is
/// the hover group its buttons follow.
pub fn disclosure<S: TimelineHost>(s: &S, line: El, d: Display, content: impl FnOnce() -> El) -> El {
    let c = col().w_full().hover_group_focus().child(line);
    if d.open() { c.child(crate::disclosure::reveal(s.knobs(), content())) } else { c }
}

/// The call's text result: what it streams while running, else what it returned.
pub fn output<'a>(c: &Call<'a>) -> &'a str {
    model::tool_output(c.status, c.metadata).unwrap_or("")
}

/// A string field of a call's input.
pub fn field<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v[k].as_str().filter(|s| !s.is_empty())
}

/// A call's path: `filePath`, else `path`.
pub fn path(input: &Value) -> Option<&str> {
    field(input, "filePath").or_else(|| field(input, "path"))
}

/// The last part of a path.
pub fn file_name(path: &str) -> &str {
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(path)
}

/// `path` inside the session's `directory`, relative to it, as Desktop's `relativizeProjectPath`.
/// On Windows paths, compares drive letters case-insensitively and treats `/` and `\` as equal
/// separators so e.g. `c:\repo` matches `C:\repo`.
pub fn relative<'a>(path: &'a str, directory: Option<&str>) -> &'a str {
    let Some(dir) = directory.filter(|d| !d.is_empty() && *d != "/" && *d != "\\") else { return path };
    let win = is_windows_path(dir);
    let dir = dir.trim_end_matches(['/', '\\']);
    let rest = if win { strip_windows_prefix(path, dir) } else { path.strip_prefix(dir) };
    match rest {
        Some("") => "",
        Some(r) => r.strip_prefix('/').or_else(|| win.then(|| r.strip_prefix('\\')).flatten()).unwrap_or(path),
        None => path,
    }
}

fn is_windows_path(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || s.contains('\\')
}

fn strip_windows_prefix<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    let (pb, db) = (path.as_bytes(), dir.as_bytes());
    if pb.len() < db.len() || !path.is_char_boundary(db.len()) {
        return None;
    }
    let drive = db.len() >= 2 && db[0].is_ascii_alphabetic() && db[1] == b':';
    for (i, (&p, &d)) in pb.iter().zip(db.iter()).enumerate() {
        let eq = if i == 0 && drive {
            p.eq_ignore_ascii_case(&d)
        } else if d == b'\\' || d == b'/' {
            p == b'\\' || p == b'/'
        } else {
            p == d
        };
        if !eq {
            return None;
        }
    }
    Some(&path[db.len()..])
}

/// `path` relative to session `session`'s directory.
pub fn session_relative<'a, S: TimelineHost>(s: &S, session: &str, path: &'a str) -> &'a str {
    relative(path, s.session_directory(session))
}

/// The directory a search ran in, relative to the session's, as its subtitle shows it; `"/"` for
/// the session's own root directory (`displayFolder`, `tool-renderer.tsx`, `#65460`).
pub fn directory<S: TimelineHost>(s: &S, session: &str, path: &str) -> Option<String> {
    let rel = session_relative(s, session, path);
    Some(if rel.is_empty() { "/".to_owned() } else { rel.to_owned() })
}

/// "1 file", "3 files" (`ui.common.file`).
pub fn files_count(n: usize) -> String {
    format!("{n} {}", opencode_i18n::plural_text(opencode_i18n::plural_key!("ui.common.file"), n as u64))
}
