//! Rewind (Settings › Developer, on by default): anything that just moved wrong. While it's on, Rune
//! records the last minute of the window's frames (`rune::rewind`): each keyed element's rect and
//! opacity, never pixels. ⌘⇧⌥R freezes the window and lays the scrubber over it, with what moved
//! in each frame outlined and the jumps marked on its track. Send writes the selected stretch as
//! a folder (summary, motion trace, contact sheet) in the app's data folder and sends it to the
//! open session as a message.

#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;

use opencode_kit::model::Message;
#[cfg(not(target_arch = "wasm32"))]
use opencode_kit::prompt::PathAttachment;
use opencode_kit::prompt::Prompt;
use rune::KeyEvent;
use rune::rewind::{Keyed, Rewind, Scrub, Sent};
use rune::{El, Tx};

use crate::palette::Pal;
use opencode_kit::model::Transcript;

use crate::{App, Route, Source};

mod story;

#[derive(Default)]
enum State {
    #[default]
    Detached,
    /// Attached to the window (`tx.record_rewind(Some(&rewind))`): recording frames.
    Recording { rewind: Rewind },
    /// Attached and frozen: scrubber overlays the window over the captured clip.
    Scrubbing { rewind: Rewind, scrub: Box<Scrub> },
}

enum Event {
    Setting(bool),
    Freeze,
    Close,
}

enum Effect {
    Record(Option<Rewind>),
    /// Only closing an open scrubber on detach needs a frame.
    Redraw,
    BlurInputs,
}

impl State {
    fn is_attached(&self) -> bool {
        !matches!(self, Self::Detached)
    }

    fn is_scrubbing(&self) -> bool {
        matches!(self, Self::Scrubbing { .. })
    }

    fn step(&mut self, event: Event) -> Vec<Effect> {
        match (std::mem::take(self), event) {
            (Self::Detached, Event::Setting(true)) => {
                let rewind = Rewind::new();
                let handle = rewind.clone();
                *self = Self::Recording { rewind };
                vec![Effect::Record(Some(handle))]
            }
            (Self::Recording { .. }, Event::Setting(false)) => {
                *self = Self::Detached;
                vec![Effect::Record(None)]
            }
            (Self::Scrubbing { mut scrub, .. }, Event::Setting(false)) => {
                scrub.close();
                *self = Self::Detached;
                // Attaching shows nothing new; only closing an open scrubber needs a frame.
                vec![Effect::Record(None), Effect::Redraw]
            }
            (Self::Recording { rewind }, Event::Freeze) => {
                let mut scrub = Box::new(Scrub::default());
                scrub.open(&rewind);
                *self = Self::Scrubbing { rewind, scrub };
                vec![Effect::BlurInputs]
            }
            (Self::Scrubbing { rewind, mut scrub }, Event::Freeze) => {
                scrub.open(&rewind);
                *self = Self::Scrubbing { rewind, scrub };
                vec![Effect::BlurInputs]
            }
            (Self::Scrubbing { rewind, mut scrub }, Event::Close) => {
                scrub.close();
                *self = Self::Recording { rewind };
                Vec::new()
            }
            (state, _) => {
                *self = state;
                Vec::new()
            }
        }
    }
}

#[derive(Default)]
pub struct Glitch {
    state: State,
    /// Closed fallback handed to `Scrub::view`'s lens only if a stale UI callback fires after close.
    closed_scrub: Scrub,
    /// Where the Dev › Rewind story's card sits (`story.rs`).
    pub demo_y: f32,
}

impl Glitch {
    pub(super) fn is_attached(&self) -> bool {
        self.state.is_attached()
    }

    pub(super) fn is_scrubbing(&self) -> bool {
        self.state.is_scrubbing()
    }

    pub(super) fn close_scrubber(&mut self) {
        self.state.step(Event::Close);
    }

    pub(super) fn scrub_mut(&mut self) -> Option<&mut Scrub> {
        match &mut self.state {
            State::Scrubbing { scrub, .. } => Some(scrub),
            State::Detached | State::Recording { .. } => None,
        }
    }
}

/// Attaches the recorder while the setting is on, and lets it go when it's turned off.
pub fn poll(s: &mut App, tx: &mut Tx) -> bool {
    let want = s.settings.record_glitches;
    if want == s.diag.glitch.is_attached() {
        return false;
    }
    let mut redraw = false;
    for effect in s.diag.glitch.state.step(Event::Setting(want)) {
        match effect {
            Effect::Record(rewind) => tx.record_rewind(rewind.as_ref()),
            Effect::Redraw => redraw = true,
            Effect::BlurInputs => {}
        }
    }
    redraw
}

fn is_chord(ev: &KeyEvent) -> bool {
    crate::keymap::Bind::Rewind.matches(ev)
}

/// ⌘⇧⌥R freezes the window (and closes the scrubber again); while it's open, it takes every key.
pub fn key(s: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if s.diag.glitch.is_scrubbing() {
        if is_chord(ev) {
            s.diag.glitch.close_scrubber();
            return true;
        }
        let outcome = match &mut s.diag.glitch.state {
            State::Scrubbing { scrub, .. } => scrub.key(ev),
            State::Detached | State::Recording { .. } => return false,
        };
        match outcome {
            Keyed::Closed => s.diag.glitch.close_scrubber(),
            Keyed::Send(sent) => send(s, sent, tx),
            _ => {}
        }
        return true;
    }
    if !is_chord(ev) {
        return false;
    }
    if !s.diag.glitch.is_attached() {
        crate::status_toast::say(s, "Rewind is off", "Turn on Rewind in Settings › Developer.");
        return true;
    }
    freeze(s);
    true
}

/// Stops recording and opens the scrubber on the last frame.
pub fn freeze(s: &mut App) {
    if !s.diag.glitch.is_attached() {
        return;
    }
    for effect in s.diag.glitch.state.step(Event::Freeze) {
        if let Effect::BlurInputs = effect {
            s.composer.focused = false;
            s.search.focused = false;
        }
    }
}

fn scrub_lens(s: &mut App) -> &mut Scrub {
    let g = &mut *s.diag.glitch;
    match &mut g.state {
        State::Scrubbing { scrub, .. } => scrub,
        State::Detached | State::Recording { .. } => &mut g.closed_scrub,
    }
}

/// The app's view, paused while the scrubber shows, with the scrubber over it. The app is wrapped
/// the same way either way, so nothing in it changes path when the scrubber opens.
pub fn wrap(s: &App, app: El) -> El {
    let (scale, overlay) = match &s.diag.glitch.state {
        State::Scrubbing { scrub, .. } => (0.0, scrub.view(scrub_lens, send)),
        State::Detached | State::Recording { .. } => (1.0, rune::el()),
    };
    rune::el()
        .w_full()
        .h_full()
        .center()
        .child(app.time_scale(scale))
        .child(overlay)
}

/// Where a bundle goes: `<data dir>/rune-opencode/glitches/<ms>`, or the temp folder in tests
/// and headless runs.
#[cfg(not(target_arch = "wasm32"))]
fn bundle_dir(s: &App) -> PathBuf {
    let base = if s.persists() { crate::dirs::data() } else { None };
    base.unwrap_or_else(|| std::env::temp_dir().join("shuvcode-rune")).join("glitches").join(crate::wall_ms().to_string())
}

/// The session the bundle goes to: the open one, or the last tab from Home.
fn target(s: &App) -> Option<String> {
    match &*s.route {
        Route::Session { id, .. } => Some(id.clone()),
        _ => s.tabs.iter().rev().find(|t| !t.is_draft()).map(|t| t.id.clone()),
    }
}

/// The message a bundle sends: its summary to read, with the trace and contact sheet attached
/// by path.
#[cfg(not(target_arch = "wasm32"))]
pub fn prompt(bundle: &rune::rewind::Bundle) -> Prompt {
    let [_, trace, contact] = bundle.files();
    let path = |p: PathBuf| p.to_string_lossy().into_owned();
    let attachments = vec![
        PathAttachment { name: "contact.png".into(), mime: "image/png".into(), path: path(contact) },
        PathAttachment { name: "trace.jsonl".into(), mime: "application/x-ndjson".into(), path: path(trace) },
    ];
    let summary = bundle.summary().trim_end().to_owned();
    Prompt {
        id: Some(opencode_kit::client::message_id()),
        text: crate::attachments::path_lines(&summary, &attachments),
        display_text: Some(summary),
        attachments,
        ..Default::default()
    }
}

/// On the web there is no local filesystem for path attachments: build the rewind summary in
/// memory from the clip's recorded stretch, jumps and moved elements and send it as the prompt.
#[cfg(target_arch = "wasm32")]
fn web_prompt(sent: &Sent) -> Prompt {
    use std::fmt::Write as _;
    let clip = sent.clip();
    let end = sent.frames().end.min(clip.len());
    let range = sent.frames().start.min(end.saturating_sub(1))..end.max(1).min(clip.len());
    let (vw, vh) = clip.viewport();
    let dt = if range.end > range.start { (clip.time(range.end - 1) - clip.time(range.start)).max(0.0) } else { 0.0 };
    let mut out = String::new();
    let note = sent.note().trim();
    if !note.is_empty() {
        let _ = writeln!(out, "{note}\n");
    }
    let _ = writeln!(out, "## Rewind ({:.0}×{:.0} pt, frames {}..{}, {:.2}s)", vw, vh, range.start, range.end, dt);
    let jumps: Vec<usize> = range.clone().filter(|&k| clip.has_jump(k)).collect();
    if !jumps.is_empty() {
        let list: Vec<String> = jumps.iter().take(12).map(usize::to_string).collect();
        let _ = writeln!(out, "- Jumps at frames: {}", list.join(", "));
    }
    let mut moved: Vec<(f32, usize, String)> = range
        .flat_map(|k| clip.changes(k).into_iter().map(move |c| (c.distance(), k, c.label())))
        .collect();
    moved.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, k, label) in moved.into_iter().take(12) {
        let _ = writeln!(out, "- frame {k}: {label}");
    }
    let summary = out.trim_end().to_owned();
    Prompt {
        id: Some(opencode_kit::client::message_id()),
        text: summary.clone(),
        display_text: Some(summary),
        ..Default::default()
    }
}

fn send(s: &mut App, sent: Sent, _: &mut Tx) {
    let Some(session) = target(s) else {
        crate::status_toast::say(s, "Open a session to send to", "The rewind stays open until you send it.");
        return;
    };
    #[cfg(not(target_arch = "wasm32"))]
    let prompt = {
        let bundle = match sent.write(&bundle_dir(s)) {
            Ok(b) => b,
            Err(e) => return crate::status_toast::fail(s, "Couldn't save the rewind", e.to_string()),
        };
        prompt(&bundle)
    };
    #[cfg(target_arch = "wasm32")]
    let prompt = web_prompt(&sent);
    let local = prompt.id.clone().unwrap_or_default();
    let display = prompt.display_text.clone().unwrap_or_default();
    if matches!(*s.source, Source::Live { .. }) {
        let busy = s.busy(&session);
        let w = s.send_write(&session, prompt);
        if !s.write(w) {
            return;
        }
        if busy {
            // The server's inbox echo shows it where it waits.
            return close(s, &session);
        }
    }
    let t = s.transcripts.entry(session.clone()).or_insert_with(|| Transcript { session: session.clone(), messages: vec![] });
    t.messages.push(Message::User { id: local, text: display });
    close(s, &session);
}

fn close(s: &mut App, session: &str) {
    s.diag.glitch.close_scrubber();
    let title = s.sessions.iter().find(|i| i.id == session).map(|i| i.title.clone()).filter(|t| !t.is_empty());
    *s.diag.status = format!("Sent to {}", title.as_deref().unwrap_or("the session"));
}

/// Settings › Developer.
pub fn settings_section(s: &App, p: &Pal) -> El {
    let on = s.settings.record_glitches;
    crate::settings::section(p, "Developer", crate::settings::settings_card(p, "developer-list", [(
        "record-glitches",
        "Rewind",
        &format!("Keep the last minute of the window. {} freezes it so you can step back to what you didn't like and send it to the session.", crate::keymap::Bind::Rewind.label()),
        crate::settings::switch(p, "record-glitches-switch", on).on_press(|s: &mut App| s.change_settings(|st| st.record_glitches = !st.record_glitches)),
    )]))
}
