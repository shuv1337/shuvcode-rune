//! Which of a session's messages Desktop holds. Desktop reads a session's context figure and its
//! Context tab off the messages its timeline has loaded (`client/src/solid/usage.ts`
//! `lastAssistantWithUsage`, `gui-plugins/src/context/tab.tsx` `data.session.message.list`), so
//! both depend on how far the reader has scrolled back:
//!
//! - opening a session reads its newest 40 messages with the Compact timeline, else 20
//!   (`app/src/runtime/server/runtime.tsx:211`, `client/src/solid/data.ts:92,1820-1856`);
//! - while the first assistant message of that read comes before any prompt, one more page of 20,
//!   three at most (`app/src/session/timeline/sync-machine.ts:6-8,138-173,221-229`);
//! - one page of 20 each time the timeline comes within 200 px of its top
//!   (`timeline/history-machine.ts:2,130,245-251`);
//! - every message created after, and nothing older until the next page;
//! - a disconnect forgets every session's pages: its next opening reads afresh (`data.ts:2150-2153`,
//!   `sync.run` keeps a finished read until then, `data.ts:228-234`).
//!
//! Rune's timeline reads in its own steps (`session_pages`: 12, then 68, then 20 per approach),
//! which paint the first screen sooner. So this keeps Desktop's arithmetic over the server's
//! messages Rune has read (their ids, kinds and times, off each page), and finds the oldest message
//! Desktop holds. The header's step and the Context tab both read from there.

use std::collections::HashMap;
use std::time::Duration;
use rune::time::Instant;

use serde_json::Value;

use crate::App;

/// One server message as the window counts it.
#[derive(Clone, Debug)]
pub struct Stub {
    id: String,
    kind: Kind,
    created: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Assistant,
    /// A `user` or `shell` message: a turn's start (`leadingTurnNeedsParent`).
    Prompt,
    Other,
}

/// Oldest server message position `(time.created, id)` in `stubs`, matching server pagination
/// order (`opencode_kit::model::parse_messages`).
pub(crate) fn oldest_position(stubs: &[Stub]) -> Option<(i64, String)> {
    stubs
        .iter()
        .filter(|s| !s.id.is_empty())
        .min_by(|a, b| (a.created, a.id.as_str()).cmp(&(b.created, b.id.as_str())))
        .map(|s| (s.created, s.id.clone()))
}

/// A page's messages as stubs, in the page's order.
pub fn stubs(page: &Value) -> Vec<Stub> {
    page["data"].as_array().into_iter().flatten().map(|m| Stub {
        id: m["id"].as_str().unwrap_or("").to_owned(),
        kind: match m["type"].as_str() {
            Some("assistant") => Kind::Assistant,
            Some("user" | "shell") => Kind::Prompt,
            _ => Kind::Other,
        },
        created: m["time"]["created"].as_i64().unwrap_or(0),
    }).collect()
}

/// Desktop's window of one session.
#[derive(Debug)]
pub struct Window {
    /// When it opened (ms since the epoch): every message created since is held.
    opened: i64,
    /// The newest page read on opening it.
    first: u32,
    /// Older pages of 20 read since: one per approach of the timeline's top.
    older: u32,
    /// The server's messages Rune has read, oldest first, and each one's place.
    stubs: Vec<Stub>,
    places: HashMap<String, usize>,
    start: Start,
    /// Rune has read back to the session's first message.
    first_read: bool,
    sync: Sync,
}

/// The oldest message Desktop holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Start {
    /// Rune hasn't read far enough back to tell (its first read is still landing).
    Unknown,
    /// `stubs[0]` and everything after it.
    At(usize),
}

/// Whether Desktop would read the session afresh at its next opening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sync {
    Current,
    /// The connection dropped since it was read.
    Forgotten,
}

/// What the Context tab and the header read of a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Held<'a> {
    /// Not known yet.
    Unknown,
    /// From this message on.
    From(&'a str),
}

impl Window {
    /// Finds the oldest message Desktop holds.
    fn locate(&mut self) {
        let complete = self.first_read;
        let before: Vec<usize> = (0..self.stubs.len()).filter(|&i| self.stubs[i].created <= self.opened).collect();
        let size = first_read(&self.stubs, &before, self.first as usize) + 20 * self.older as usize;
        let n = before.len();
        self.start = if self.stubs.is_empty() || (size >= n && !complete) {
            // Desktop's read reaches as far back as Rune has read, and there may be more.
            Start::Unknown
        } else if size >= n {
            Start::At(0)
        } else {
            Start::At(before[n - size])
        };
    }

    fn holds(&self, id: &str) -> bool {
        match self.start {
            Start::Unknown => true,
            Start::At(from) => self.places.get(id).is_none_or(|&at| at >= from),
        }
    }

    /// Takes a page's messages (newest first): an older page's go before, a newer one's after.
    fn read(&mut self, page: &[Stub], older: bool) {
        let fresh: Vec<Stub> = page.iter().rev().filter(|s| !s.id.is_empty() && !self.places.contains_key(&s.id)).cloned().collect();
        if fresh.is_empty() {
            return;
        }
        if older {
            self.stubs.splice(0..0, fresh);
        } else {
            self.stubs.extend(fresh);
        }
        self.places = self.stubs.iter().enumerate().map(|(i, s)| (s.id.clone(), i)).collect();
    }
}

/// Desktop's first read of `before` (indices into `stubs`, oldest first): `first` of the newest,
/// then pages of 20, three at most, while the first assistant message it read comes before any
/// prompt (`sync-machine.ts` `resolveLeadingAssistant`, `leadingTurnNeedsParent`).
fn first_read(stubs: &[Stub], before: &[usize], first: usize) -> usize {
    let n = before.len();
    let mut size = first.min(n);
    let Some(assistant) = before[n - size..].iter().copied().find(|&i| stubs[i].kind == Kind::Assistant) else { return size };
    let needs_parent = |size: usize| {
        let read = &before[n - size..];
        let prompt = read.iter().position(|&i| stubs[i].kind == Kind::Prompt);
        read.iter().position(|&i| i == assistant).is_some_and(|at| prompt.is_none_or(|p| at < p))
    };
    for _ in 0..3 {
        if size >= n || !needs_parent(size) {
            break;
        }
        size = (size + 20).min(n);
    }
    size
}

/// Each session's window, by (server, session), and where the reader stood at the last poll.
#[derive(Default)]
pub struct Windows {
    map: HashMap<(String, String), Window>,
    reader: Reader,
}

impl Windows {
    pub fn retain(&mut self, server: &str, keep: &dyn Fn(&str) -> bool) {
        self.map.retain(|(s, id), _| s != server || keep(id));
    }

    pub fn forget(&mut self, server: &str, id: &str) {
        self.map.remove(&(server.to_owned(), id.to_owned()));
    }
}

/// Where the open timeline's reader stands, so each approach of its top counts once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Reader {
    /// Away from the top.
    #[default]
    Away,
    /// Within 200 px of session `0`'s top.
    Near(String),
    /// A page of Rune's own landed above the reader (`session_pages` then asks again where the
    /// reader stands): still near the top in the next moment is the same approach, as Desktop's
    /// own page would have moved the reader down.
    Landed { id: String, at: Instant },
}

/// How long after Rune's page lands its report of the reader's place may take.
const SETTLE: Duration = Duration::from_millis(300);

impl Reader {
    /// The reader's place at a poll (`near`: the session whose top it stands near, empty for none):
    /// the next state, and whether it is a new approach.
    fn step(&self, near: &str, now: Instant) -> (Reader, bool) {
        match (self, near.is_empty()) {
            (Reader::Landed { at, .. }, true) if now.duration_since(*at) < SETTLE => (self.clone(), false),
            (_, true) => (Reader::Away, false),
            (Reader::Near(id), false) if id == near => (self.clone(), false),
            (Reader::Landed { id, at }, false) if id == near && now.duration_since(*at) < SETTLE => (Reader::Near(near.to_owned()), false),
            (_, false) => (Reader::Near(near.to_owned()), true),
        }
    }
}

impl App {
    /// Session `id` opened: its window starts now, unless this window holds it from an earlier
    /// opening on the same connection, as Desktop's store keeps a session's pages.
    pub(crate) fn window_opened(&mut self, id: &str) {
        let first = if self.settings.timeline_detail.preset() == Some(COMPACT) { 40 } else { 20 };
        let key = (server(self), id.to_owned());
        let opened = *self.now_ms;
        let w = self.context_windows.map.entry(key).or_insert_with(|| Window { opened, first, older: 0, stubs: vec![], places: HashMap::new(), start: Start::Unknown, first_read: false, sync: Sync::Current });
        if w.sync == Sync::Forgotten {
            (w.opened, w.first, w.older, w.sync) = (opened, first, 0, Sync::Current);
        }
        w.locate();
    }

    /// A page of session `id`'s messages Rune has read (`stubs`, newest first); `first` when it
    /// reached the session's first message (no cursor past it, or fewer than it asked for: the
    /// server hands out a cursor at the end too).
    pub(crate) fn window_read(&mut self, id: &str, stubs: &[Stub], older: bool, first: bool) {
        let key = (server(self), id.to_owned());
        let Some(w) = self.context_windows.map.get_mut(&key) else { return };
        w.read(stubs, older);
        w.first_read |= first && (older || w.stubs.len() == stubs.len());
        w.locate();
    }

    /// The connection came back: each session reads afresh at its next opening. The open one
    /// keeps what it holds until then, as Desktop's open timeline does.
    pub(crate) fn windows_forgotten(&mut self) {
        for w in self.context_windows.map.values_mut() {
            w.sync = Sync::Forgotten;
        }
    }

    /// Each poll: the open session's timeline coming near its top reads Desktop's next page.
    pub(crate) fn poll_context_window(&mut self) -> bool {
        let (reader, approached) = self.context_windows.reader.step(&self.pages.near_top(), Instant::now());
        if reader == self.context_windows.reader {
            return false;
        }
        self.context_windows.reader = reader;
        let Reader::Near(id) = &self.context_windows.reader else { return false };
        // Rune paints a short latest page before the history behind it; standing at the top of
        // that is no reader's approach.
        let read = self.pages.first_read_done(id);
        let key = (server(self), id.clone());
        match self.context_windows.map.get_mut(&key) {
            Some(w) if approached && read => {
                w.older += 1;
                w.locate();
                true
            }
            _ => false,
        }
    }

    /// A page of Rune's own landed above session `id`'s reader.
    pub(crate) fn context_window_landed(&mut self, id: &str) {
        self.context_windows.reader = Reader::Landed { id: id.to_owned(), at: Instant::now() };
    }

    /// What Desktop holds of session `id`: none before this window opened it (all of it counts).
    pub fn held(&self, id: &str) -> Option<Held<'_>> {
        let w = self.context_windows.map.get(&(server(self), id.to_owned()))?;
        Some(match w.start {
            Start::Unknown => Held::Unknown,
            Start::At(from) => Held::From(&w.stubs[from].id),
        })
    }

    /// Whether Desktop holds message `message` of session `id`. A message Rune hasn't read in a
    /// page (one that arrived since) is held.
    pub fn holds(&self, id: &str, message: &str) -> bool {
        self.context_windows.map.get(&(server(self), id.to_owned())).is_none_or(|w| w.holds(message))
    }
}

fn server(app: &App) -> String {
    crate::servers::runtime::active(app).as_str().to_owned()
}

/// `session_view::detail::PRESETS`' Compact.
const COMPACT: usize = 2;
