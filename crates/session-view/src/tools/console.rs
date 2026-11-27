//! Shell: "Shell" and its command; open, Desktop's console card (`ConsoleOutput`) of the command
//! over its output, which Execute's output row opens too.

use rune::{Color, El, Tx, col, el, row, text};

use super::plain::one_line;
use super::log::{self, Excerpt};
use super::{Call, Ctx, Display, Phase, Press, Trigger, body, disclosure, field, output, trigger};
use crate::TimelineHost;
use crate::focus::FocusExt;
use crate::palette::Pal;
use crate::style::{INSTANT, TOOL_CODE, readable};

/// Desktop caps a console, a call's input and an error at 240 px and scrolls the rest.
pub const MAX_H: f32 = 240.0;

/// A shell call: "Shell" and its command on one line; open, the console. It opens while it
/// runs, its output streaming in.
pub fn shell<S: TimelineHost>(s: &S, p: &Pal, cx: Ctx, c: &Call) -> El {
    let d = Display::of(s, cx, c, true);
    let command = field(c.input, "command").or_else(|| field(c.metadata, "command")).unwrap_or("");
    let out = current_output(s, c);
    // While open the command shows in the console, so the trigger drops it.
    let writing = command.is_empty() && d.phase == Phase::Pending;
    let subtitle = match (d.open() && out.len() <= log::INLINE, writing) {
        (true, _) => None,
        (false, true) => Some(opencode_i18n::t!("ui.tool.shell.writingCommand").to_owned()),
        (false, false) => Some(one_line(command)),
    };
    let t = Trigger { title: opencode_i18n::t!("ui.tool.shell").into(), compact: true, subtitle, code: !writing, tag: outcome(c), ..Default::default() };
    if let Excerpt::Ends {first, last, bytes} = Excerpt::new(out) {
        // Size and the recovery action are useful even while shut. Opening this card changes
        // only its two actual excerpts, never mounts another scrolling console or its chrome.
        let partial = c.metadata["truncated"].as_bool()==Some(true) || d.phase!=Phase::Completed;
        let measure = format!("{}{} B", if partial { "≥ " } else { "" }, opencode_i18n::integer(bytes as i64));
        let label = opencode_i18n::t!("rune.scale.tooLarge", size = measure);
        let save = if partial {format!("{} · {}",opencode_i18n::t!("ui.message.copy"),opencode_i18n::t!("plugin.file.view.preview"))} else {opencode_i18n::t!("rune.scale.save").into()};
        let (session,id)=(cx.session.to_owned(),c.id.to_owned());
        let action=row().key(if partial {"copy-preview"} else {"save-raw"}).role("button").label(save.clone()).focusable(true).focus_default(p).px(8.0).min_h(28.0).center().radius(6.0).fill(p.layer2).hover(p.hover)
            .on_press_tx(move |s:&mut S,tx| if partial {if let Some(value)=super::raw_output(s,&session,&id){tx.copy(value);}} else {s.save_tool_output(&session,&id,tx)})
            .child(text(save).font(crate::style::UI_MED).color(p.text));
        let summary = row().key("size").w_full().gap(8.0).center_y().child(text(label).font(crate::style::UI).color(p.muted).wrap()).child(action);
        let mut card=col().key("large-output").w_full().padding(12.0).gap(4.0).fill(p.bg_base).child(summary);
        if d.open() {
            let head=super::plain::clean(first);
            let tail=super::plain::clean(last);
            let excerpt = readable(p,text(format!("{head}\n…\n{tail}")).font(TOOL_CODE).color(p.muted).wrap_anywhere()).key("excerpt").w_full();
            card=card.child(crate::disclosure::reveal(s.knobs(),excerpt));
        }
        return col().w_full().child(trigger(s,p,cx,c,d,t,Press::Toggle(super::key(c.id)),None)).child(card);
    }
    // Open, the console starts the small gap under the trigger's line, which ends 6 above the
    // trigger's foot (`timeline-next.css:395-402`, `:486-494`).
    disclosure(s, trigger(s, p, cx, c, d, t, Press::Toggle(super::key(c.id)), None), d, || {
        body(console(s, p, cx.session, c.id, (!command.is_empty()).then_some(command), out), false, crate::timeline::rhythm::S - 6.0)
    })
}

/// How a command ended, where it did not succeed (`shellOutcome`): stopped, timed out, or the
/// exit code.
fn outcome(c: &Call) -> Option<(String, bool)> {
    let m = c.metadata;
    if m["interrupted"] == true || m["status"] == "killed" || crate::activity::rules::stopped(c.status, m) {
        return Some((opencode_i18n::t!("ui.timeline.shell.stopped").to_owned(), false));
    }
    if m["timeout"] == true || m["status"] == "timeout" {
        return Some((opencode_i18n::t!("ui.timeline.shell.timedOut").to_owned(), true));
    }
    m["exit"].as_i64().filter(|e| *e != 0).map(|e| (opencode_i18n::t!("ui.timeline.shell.exit", code = e), true))
}

/// The shell a shell call's console reads its output from, and whether it reads it again each
/// second, as Desktop's `run-machine.ts` decides: a call the model is still writing, or whose
/// result carries the shell's final output (`completed`, `exited`, `timeout`, `killed`), reads
/// nothing; one running, or whose shell the server still lists as running (`running`), polls;
/// any other reads once. None for a call with no shell.
pub fn follow<'a>(c: &Call<'a>, running: impl Fn(&str) -> bool) -> Option<(&'a str, bool)> {
    let id = field(c.metadata, "shellID")?;
    let saved = c.metadata["status"].as_str().is_some_and(|s| matches!(s, "completed" | "exited" | "timeout" | "killed"));
    if saved || *c.status == opencode_kit::model::ToolStatus::Pending {
        return None;
    }
    Some((id, *c.status == opencode_kit::model::ToolStatus::Running || running(id)))
}

/// Whether shell call `c`'s console shows: Desktop reads a shell's output only while its
/// console is mounted, as its card is open.
pub fn console_open<S: TimelineHost>(s: &S, c: &Call) -> bool {
    crate::disclosure::is_open(s, &super::key(c.id), super::default_open(s, c))
}

/// The inset of a section's text, kept while it scrolls.
const PAD: f32 = 12.0;
const RADIUS: f32 = 6.0;
/// The ring's width in CSS px (`message-part.css:806`): it lays out on whole device pixels and the
/// sections sit inside it, rounded concentrically.
const RING: f32 = 0.5;

/// Desktop's console card (`ConsoleOutput`, `message-part.css:801-918`): the command over its
/// result, each with a copy button, scrolling together inside one region capped at [`MAX_H`]
/// (`bash-scroll`, no scroll bar). The ring stays whole round all four corners, the scroll
/// viewport clipped concentrically inside it.
pub fn console<S: TimelineHost>(s: &S, p: &Pal, session: &str, id: &str, command: Option<&str>, out: &str) -> El {
    // A terminal reads left to right in any window; its Copy buttons stay at its right.
    // `border: 0.5px solid border-base` (`message-part.css:806`) takes its device pixel in layout, as
    // Chromium's does, so the sections' fills never round onto it at a fractional offset.
    let ring = crate::scroller::border_width(RING);
    let card = col()
        .key("console")
        .dir(rune::Direction::Ltr)
        .w_full()
        .max_h(MAX_H + 2.0 * ring)
        .radius(RADIUS)
        .border(RING, p.border)
        .border_layout()
        .fill(p.bg_base)
        .hover_group_focus()
        // Its Copy fades in and out over 0.15 s ease (`message-part.css:796-800`'s `bash-copy`).
        .hover_motion(crate::style::HOVER_FADE, crate::style::HOVER_FADE);
    let inner = (RADIUS - ring).max(0.0);
    let result = !out.is_empty();
    let mut stack = col().key("bash-body").w_full();
    if let Some(cmd) = command {
        let words = text(cmd.to_owned()).font(TOOL_CODE).color(p.text).wrap_anywhere();
        stack = stack.child(section(s, p, session, id, Part { name: "command", fill: p.bg_base, label: opencode_i18n::t!("ui.message.copy"), rule: false }, words));
    }
    if result {
        // A line break that ends the output opens no empty line, as in a `pre`.
        let words = output_view::<S>(p, session, id, out, true);
        stack = stack.child(section(s, p, session, id, Part { name: "result", fill: p.layer1, label: opencode_i18n::t!("ui.message.copyOutput"), rule: command.is_some() }, words));
    }
    // `bash-scroll`: one region at most `MAX_H` tall scrolling command and output together, with
    // no scroll bar (`message-part.css:727-730,815-824`, `tool-renderer.tsx:1988`, `opencode#65785`):
    // click-focusable for keyboard scrolling (`tab_index(-1)`), not a Tab stop, and draws no ring.
    let viewport = crate::scroller::scroller("scroll").hidden(
        col()
            .w_full()
            .max_h(MAX_H)
            .shrink()
            .min_h(0.0)
            .corners([inner; 4])
            .clip_corners([inner; 4])
            .focusable(true)
            .tab_index(-1)
            .no_focus_ring()
            .role("region")
            .label_static(opencode_i18n::t!("ui.scrollView.ariaLabel")),
        stack,
    );
    card.child(viewport)
}

/// Resolve by identity on a user action; mounting the card never clones a whole raw output.
pub fn with_call<S: TimelineHost, R>(s: &S, session: &str, id: &str, f: impl FnOnce(Call<'_>) -> R) -> Option<R> {
    use opencode_kit::model::{Message, Part};
    s.with_transcript(session, |t| {
        for m in &t.messages {
            let a = match m { Message::Assistant(a) => a, Message::Note { step, .. } => step, _ => continue };
            for part in &a.parts {
                if let Part::Tool { id: key, name, status, input, metadata, .. } = part && key == id {
                    return Some(f(Call { id: key, name, status, input, metadata }));
                }
            }
        }
        None
    }).flatten()
}

pub fn current_output<'a, S: TimelineHost>(s: &'a S, c: &Call<'a>) -> &'a str {
    follow(c, |_| false).and_then(|(id, _)| s.shell_output(id)).filter(|s| !s.is_empty()).unwrap_or_else(|| output(c))
}

pub fn output_view<S: TimelineHost>(p: &Pal, session: &str, id: &str, raw: &str, shell: bool) -> El {
    let (font, ink, lh) = if shell { (TOOL_CODE, p.muted, 20.0) } else { (crate::style::MD, p.text, 22.4) };
    let view = |t: &str| log::visible::<S>(p, t, font, ink, shell, lh);
    match Excerpt::new(raw) {
        Excerpt::Whole(t) => view(t),
        Excerpt::Ends { first, last, bytes } => {
            let (session, id) = (session.to_owned(), id.to_owned());
            let save = opencode_i18n::t!("rune.scale.save");
            let notice = opencode_i18n::t!("rune.scale.tooLarge", size = format!("{} B", opencode_i18n::integer(bytes as i64)));
            col().w_full().padding(12.0).radius(6.0).fill(p.bg_base).border(0.5,p.ring).gap(4.0)
                .child(row().w_full().gap(8.0).center_y().child(text(notice).font(crate::style::UI).color(p.muted).wrap())
                    .child(row().key("save-raw").role("button").focusable(true).focus_default(p).label_static(save).px(8.0).min_h(28.0).center().radius(6.0).fill(p.layer2).hover(p.hover)
                        .on_press_tx(move |s: &mut S, tx| s.save_tool_output(&session, &id, tx)).child(text(save).font(crate::style::UI_MED).color(p.text))))
                .child(log::visible::<S>(p, &format!("{first}\n…\n{last}"),font,ink,shell,lh))
        }
    }
}

/// `content` in a region at most [`MAX_H`] tall that scrolls the rest, with no scroll bar, as Desktop hides it
/// (`message-part.css:746-755`).
pub fn capped(content: El) -> El {
    // `tool-output` (`tool-renderer.tsx:1725,1758,1796`, `message-part.css:727-730`, `opencode#65785`):
    // click-focusable for keyboard scrolling (`tab_index(-1)`), not a Tab stop, and draws no ring.
    crate::scroller::scroller("scroll").hidden(
        col()
            .w_full()
            .max_h(MAX_H)
            .focusable(true)
            .tab_index(-1)
            .no_focus_ring()
            .role("region")
            .label_static(opencode_i18n::t!("ui.scrollView.ariaLabel")),
        content,
    )
}

/// One section of a console.
struct Part {
    name: &'static str,
    fill: Color,
    label: &'static str,
    /// A hairline over it, as it follows the command.
    rule: bool,
}

/// One section of the console: its text on its fill, 12 pt in from each edge, and its copy
/// button 4 pt in from the top right. A backdrop of the section's fill sits behind the button,
/// fading out to its left, top and bottom, so text under it softens rather than being cut.
fn section<S: TimelineHost>(s: &S, p: &Pal, session: &str, id: &str, part: Part, words: El) -> El {
    let copy_key = format!("copy-tool-{id}-{}", part.name);
    let (session, id, field_name, fill, label) = (session.to_owned(), id.to_owned(), part.name, part.fill, part.label);
    let done = crate::copied::is_copied(s, &copy_key);
    let button = row()
        .key("copy")
        .role("button")
        .label(if done { opencode_i18n::t!("ui.message.copied") } else { label })
        .help(if done { opencode_i18n::t!("ui.message.copied") } else { label })
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
                let copied = with_call(s, &session, &id, |c| match field_name {
                    "command" => field(c.input, "command").or_else(|| field(c.metadata, "command")).unwrap_or("").to_owned(),
                    _ => super::plain::clean(current_output(s, &c)).into_owned(),
                });
                if let Some(copied) = copied { tx.copy(copied); crate::copied::mark_copied(s, copy_key.clone(), tx); }
            }
        })
        .child(crate::copied::glyph(s, &copy_key, 14.0, p.icon_muted));
    let rule_h = if part.rule { crate::scroller::border_width(RING) } else { 0.0 };
    let backdrop = el().key("backdrop").absolute(-24.0, -4.0).size(52.0, 36.0).clip_fade_linear(24.0, 8.0, 0.0, 8.0).child(el().w_full().h_full().fill(fill));
    let holder = crate::style::hover_reveal(el().key("copy-holder").size(24.0, 24.0).pin(None, Some(4.0 + rule_h), Some(4.0), None)).child(backdrop).child(button);
    // `border-block-start: 0.5px` (`message-part.css:901`) takes one snapped device pixel above
    // `bash-pre`'s 12 pt padding (`message-part.css:837`).
    let inset = col().key("text").w_full().pt(PAD + rule_h).px(PAD).pb(PAD).child(readable(p, words).w_full());
    let mut sec = col().key(part.name).w_full().fill(fill).child(inset).child(holder);
    // `border-block-start: 0.5px solid border-base` (`message-part.css:901`): one whole device
    // pixel, drawn after the copy button's backdrop, which stops below it.
    if part.rule {
        sec = sec.child(el().key("rule").absolute(0.0, 0.0).w_full().h(rule_h).fill(p.border));
    }
    sec
}
