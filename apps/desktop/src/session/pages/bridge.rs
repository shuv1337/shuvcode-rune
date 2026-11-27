//! A transcript never skips messages (`packages/client/src/solid/data.ts` `session.message.sync`,
//! opencode#66035; the rules under "Transcript reads" in `packages/app/AGENTS.md`).
//!
//! The newest page is read again on every reconnect of the event stream. When more than a page
//! arrived while its events were missed, that page does not reach the loaded messages: joined to
//! them by id it would leave the messages between out, and nothing would show the reader that
//! they are missing. So it is held, and older pages are read until they meet what is loaded
//! ([`Bridge`]); after a longer absence the loaded messages go and only the newest stay, as on a
//! first read. A loaded message that was still running when its events were missed is read
//! again by itself.

use opencode_kit::model::{CompactionStatus, Message, Part, ToolStatus, Transcript};

use super::machines::{Bridge, BridgeEffect, BridgeEvent, HistoryEvent};
use super::{Job, Page};
use crate::App;

/// Which read a page answers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ReadOf {
    /// The session's newest messages.
    #[default]
    Newest,
    /// The messages before this cursor.
    Before(String),
    /// One message, read again.
    One,
}

impl ReadOf {
    pub(super) fn of(cursor: Option<&str>, one: bool) -> Self {
        match cursor {
            None => Self::Newest,
            Some(_) if one => Self::One,
            Some(cursor) => Self::Before(cursor.to_owned()),
        }
    }
}

/// A newest page held back while older pages are read toward the loaded messages.
pub(super) struct Held {
    bridge: Bridge,
    /// The newest page as it landed.
    newest: Page,
    /// Every message read so far, oldest first.
    messages: Vec<Message>,
}

impl Held {
    pub(super) fn awaits(&self, cursor: &str) -> bool {
        matches!(&self.bridge, Bridge::Reading { cursor: awaited, .. } if awaited == cursor)
    }
}

/// How many loaded messages that were still running are read again after one read.
const REFRESH: usize = 16;

impl App {
    /// A page landed: into the transcript, unless it has to wait for the pages between it and
    /// the loaded messages.
    pub(super) fn merge_page(&mut self, page: &Page, tx: &mut rune::Tx) {
        let id = page.key.id.clone();
        match &page.read_of {
            ReadOf::One => {
                if let Some(t) = self.transcripts.get_mut(&id) {
                    t.merge(page.messages.clone());
                }
            }
            ReadOf::Before(cursor) if self.pages.bridges.get(&id).is_some_and(|h| h.awaits(cursor)) => {
                let Some(held) = self.pages.bridges.remove(&id) else { return };
                let mut messages = page.messages.clone();
                messages.extend(held.messages);
                let meets = self.transcripts.get(&id).is_none_or(|t| !apart(t, &messages));
                self.step_bridge(Some(&held.bridge), held.newest, messages, BridgeEvent::Landed { meets, cursor: page.cursor.clone() }, tx);
            }
            ReadOf::Before(cursor) if self.pages.awaits_history(&id, cursor) => self.merge_joined(page, tx),
            ReadOf::Before(_) => {}
            ReadOf::Newest => {
                // A newer read takes the place of one still on its way to the loaded messages.
                self.pages.bridges.remove(&id);
                let cold = self.launch.cold_transcript.as_ref().is_some_and(|c| c.id == id);
                let stale = self.page_key(&id).updated > page.key.updated;
                if !cold && !stale && self.transcripts.get(&id).is_some_and(|t| apart(t, &page.messages)) {
                    return self.step_bridge(None, page.clone(), page.messages.clone(), BridgeEvent::Apart { cursor: page.cursor.clone() }, tx);
                }
                self.merge_joined(page, tx);
                self.read_unsettled(page);
            }
        }
    }

    /// Steps the bridge of `newest`'s session; `messages` is every message read for it so far.
    fn step_bridge(&mut self, state: Option<&Bridge>, newest: Page, messages: Vec<Message>, event: BridgeEvent, tx: &mut rune::Tx) {
        let id = newest.key.id.clone();
        // What stays above the pages read, when they replace the loaded messages.
        let above = match &event {
            BridgeEvent::Apart { cursor } | BridgeEvent::Landed { cursor, .. } => cursor.clone(),
        };
        let (next, effects) = Bridge::step(state, event);
        let mut landed = None;
        for effect in effects {
            match effect {
                BridgeEffect::Read { cursor, limit } => {
                    if let Some(pool) = &self.pages.pool { pool.history(newest.key.clone(), cursor, limit); }
                }
                BridgeEffect::Join => landed = Some(false),
                BridgeEffect::Replace => landed = Some(true),
            }
        }
        match (next, landed) {
            (Some(bridge), _) => {
                self.pages.bridges.insert(id, Held { bridge, newest, messages });
            }
            (None, Some(replace)) => {
                // What is newer than everything read stays (a live delta, a prompt just sent); the
                // loaded messages before the pages go when the pages do not reach them.
                if replace && let (Some(t), Some(last)) = (self.transcripts.get_mut(&id), messages.last()) {
                    t.messages.retain(|m| m.id() > last.id());
                }
                let page = Page { messages, ..newest };
                self.merge_joined(&page, tx);
                if replace {
                    // Older pages continue above the last one read; the timeline is at its end.
                    self.step_history(&id, HistoryEvent::Replaced { cursor: above }, tx);
                    session_view::jump::settle(self, tx);
                }
                self.read_unsettled(&page);
            }
            (None, None) => {}
        }
    }

    /// The loaded messages before `page`'s that were still running: events that changed them may
    /// have been missed, so each is read again (`data.ts` `unsettled`).
    fn read_unsettled(&self, page: &Page) {
        let (Some(pool), Some(t), Some(first)) = (&self.pages.pool, self.transcripts.get(&page.key.id), page.messages.first()) else { return };
        for m in t.messages.iter().filter(|m| m.id() < first.id() && unsettled(m)).take(REFRESH) {
            pool.enqueue(Job { key: page.key.clone(), cursor: Some(m.id().to_owned()), limit: 1, foreground: false, one: true });
        }
    }
}
/// Whether messages lie between the loaded transcript and a newer read of it, both oldest
/// first: neither starts inside the other (`data.ts` `apart`). A transcript with nothing older
/// than the read has nothing to be apart from.
fn apart(loaded: &Transcript, fetched: &[Message]) -> bool {
    let (Some(oldest), Some(first)) = (loaded.messages.first(), fetched.first()) else { return false };
    oldest.id() < first.id() && !loaded.messages.iter().any(|m| m.id() == first.id()) && !fetched.iter().any(|m| m.id() == oldest.id())
}

/// A message that later events still change (`data.ts` `unsettled`).
fn unsettled(message: &Message) -> bool {
    match message {
        Message::Assistant(a) => {
            (a.completed.is_none() && a.error.is_none()) || a.parts.iter().any(|p| matches!(p, Part::Tool { status: ToolStatus::Pending | ToolStatus::Running, .. }))
        }
        Message::Note { step, .. } => step.parts.iter().any(|p| matches!(p, Part::Tool { status: ToolStatus::Running, .. })),
        Message::Compaction(c) => c.status == CompactionStatus::Running,
        Message::User { .. } | Message::Notice(_) => false,
    }
}
