//! A session's undelivered prompts: queued or steering, as the server delivers them.

use super::*;

/// How the server delivers a prompt sent while the session is busy: `Steer` joins the running
/// turn at its next step boundary; `Queue` waits for the turn (and every steer) to finish. The
/// server steers when a prompt names neither.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Delivery {
    #[default]
    Steer,
    Queue,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Steer => "steer",
            Delivery::Queue => "queue",
        }
    }

    pub fn parse(v: &Value) -> Option<Delivery> {
        match v.as_str()? {
            "steer" => Some(Delivery::Steer),
            "queue" => Some(Delivery::Queue),
            _ => None,
        }
    }
}

/// A prompt the server admitted but hasn't delivered yet (`GET /api/session/:id/inbox`). Its id
/// becomes the user message's id once delivered.
#[derive(Clone, Debug, PartialEq)]
pub struct InboxItem {
    pub id: String,
    pub text: String,
    pub delivery: Delivery,
    pub files: Vec<Attachment>,
}

/// One user item, from the inbox list (`{id, type, payload, delivery}`) or a
/// `session.inbox.enqueued` event (`{inboxID, item: {type, payload, delivery}}`). Synthetic,
/// compaction and move items are None.
pub fn parse_inbox_item(v: &Value) -> Option<InboxItem> {
    let (id, item) = if v["item"].is_object() { (&v["inboxID"], &v["item"]) } else { (&v["id"], v) };
    if item["type"] != "user" {
        return None;
    }
    let payload = &item["payload"];
    let mut text = payload["metadata"]["displayText"].as_str().filter(|t| !t.is_empty()).map_or_else(|| s(&payload["text"]), str::to_owned);
    // With a presentation (`readPromptPresentation`) it reads as typed, its quotes and the files
    // it names by path showing as cards, even when it holds no words of its own.
    if let Some((display, comments)) = crate::quote::parse_metadata(&payload["metadata"]) {
        let refs = crate::mention::path_attachments(&payload["metadata"]).unwrap_or_default();
        if display.is_empty() && (!refs.is_empty() || !comments.is_empty()) {
            text = display;
        }
        crate::mention::set_user_path_attachments(&s(id), refs);
        set_user_comments(&s(id), comments);
    }
    let delivery = Delivery::parse(&item["delivery"])?;
    inbox_payload_store().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(s(id), payload.clone());
    Some(InboxItem { id: s(id), text, delivery, files: parse_attachments(&payload["files"]) })
}

/// Undelivered prompts' payloads as the server holds them, by inbox id. `InboxItem` has no field
/// for it, so apps that build one as a literal keep compiling; editing a queued prompt reads it.
fn inbox_payload_store() -> &'static Mutex<HashMap<String, Value>> {
    static STORE: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
    STORE.get_or_init(Default::default)
}

/// The payload of inbox item `id`, once listed or admitted.
pub fn inbox_payload(id: &str) -> Option<Value> {
    inbox_payload_store().lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(id).cloned()
}

pub(crate) fn retain_inbox_payloads(keep: &dyn Fn(&str) -> bool) -> bool {
    let mut store = inbox_payload_store().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = store.len();
    store.retain(|id, _| keep(id));
    store.len() != before
}

/// What [`Inbox::apply`] did.
#[derive(Debug, PartialEq)]
pub enum InboxChange {
    None,
    Changed,
    /// The item left the inbox for the transcript.
    Delivered(InboxItem),
}

/// A background shell or subagent's notice on its way to the model (`visibleTimelineMessages`):
/// it waits in the inbox, showing under the running work, until the server delivers it to the
/// transcript, where it keeps its row, or cancels it. A listing without it means it went on.
#[derive(Clone, Debug, PartialEq)]
pub enum NoticeState {
    Waiting(Notice),
    Delivered,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NoticeEvent {
    /// Admitted, or admitted again with a newer payload.
    Admitted(Notice),
    Delivered,
    Cancelled,
}

/// What the transcript does for a notice's step: take it as a message of its own.
#[derive(Clone, Debug, PartialEq)]
pub enum NoticeEffect {
    ToTranscript(Notice),
}

/// One notice's transition.
pub fn step_notice(state: NoticeState, event: NoticeEvent) -> (NoticeState, Option<NoticeEffect>) {
    match (state, event) {
        (NoticeState::Waiting(_), NoticeEvent::Admitted(n)) => (NoticeState::Waiting(n), None),
        (NoticeState::Waiting(n), NoticeEvent::Delivered) => (NoticeState::Delivered, Some(NoticeEffect::ToTranscript(n))),
        (NoticeState::Waiting(_), NoticeEvent::Cancelled) => (NoticeState::Cancelled, None),
        (done, _) => (done, None),
    }
}

/// One session's undelivered prompts, in admission order, kept current from
/// `session.inbox.*` events.
#[derive(Clone, Debug, Default)]
pub struct Inbox {
    pub items: Vec<InboxItem>,
    /// True once a full server inbox listing (`replace`) has synced this session's inbox.
    pub synced: bool,
    /// The id of a compaction the server admitted but hasn't started.
    pub compaction: Option<String>,
    /// Background shells and subagents that came back, in admission order: the notices
    /// Desktop shows for them under the running work while they wait ([`NoticeState`]).
    notices: Vec<NoticeState>,
    /// The effects of the notices' steps, for the caller to take ([`Inbox::take_notice_effects`]).
    notice_effects: Vec<NoticeEffect>,
}

impl Inbox {
    /// The notices still waiting, in admission order.
    pub fn waiting_notices(&self) -> impl Iterator<Item = &Notice> {
        self.notices.iter().filter_map(|n| match n {
            NoticeState::Waiting(n) => Some(n),
            _ => None,
        })
    }

    /// The waiting notices `seen` says the transcript already holds were delivered.
    pub fn settle_notices(&mut self, seen: impl Fn(&str) -> bool) {
        let ids: Vec<String> = self.waiting_notices().filter(|n| seen(&n.id)).map(|n| n.id.clone()).collect();
        for id in ids {
            self.step_notice(&id, NoticeEvent::Delivered);
        }
    }

    pub fn take_notice_effects(&mut self) -> Vec<NoticeEffect> {
        std::mem::take(&mut self.notice_effects)
    }

    /// Steps the waiting notice `id` (or a new one, when admitted), dropping it once it's done.
    fn step_notice(&mut self, id: &str, event: NoticeEvent) {
        let at = self.notices.iter().position(|n| matches!(n, NoticeState::Waiting(w) if w.id == id));
        let state = match (at, event) {
            (Some(at), event) => (self.notices.remove(at), event),
            // Admitted for the first time: it starts out waiting.
            (None, NoticeEvent::Admitted(n)) => return self.notices.push(NoticeState::Waiting(n)),
            (None, _) => return,
        };
        let (state, event) = state;
        let (next, effect) = step_notice(state, event);
        self.notice_effects.extend(effect);
        if matches!(next, NoticeState::Waiting(_)) {
            self.notices.insert(at.unwrap_or(self.notices.len()), next);
        }
    }

    pub fn queued(&self) -> impl Iterator<Item = &InboxItem> {
        self.items.iter().filter(|i| i.delivery == Delivery::Queue)
    }

    pub fn steering(&self) -> impl Iterator<Item = &InboxItem> {
        self.items.iter().filter(|i| i.delivery == Delivery::Steer)
    }

    /// Adds `item`, or replaces the one with its id in place.
    pub fn upsert(&mut self, item: InboxItem) {
        match self.items.iter_mut().find(|i| i.id == item.id) {
            Some(slot) => *slot = item,
            None => self.items.push(item),
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<InboxItem> {
        let at = self.items.iter().position(|i| i.id == id)?;
        Some(self.items.remove(at))
    }

    pub fn set_delivery(&mut self, id: &str, delivery: Delivery) -> bool {
        self.items.iter_mut().find(|i| i.id == id).is_some_and(|i| std::mem::replace(&mut i.delivery, delivery) != delivery)
    }

    /// Reconcile the complete inbox returned by the server.
    pub fn replace(&mut self, rows: &[Value]) {
        self.synced = true;
        self.compaction = rows.iter().find(|i| i["type"] == "compaction").map(|i| s(&i["id"]));
        let listed: Vec<Notice> = rows.iter().filter_map(parse_inbox_notice).collect();
        let gone: Vec<String> = self.waiting_notices().filter(|n| !listed.iter().any(|l| l.id == n.id)).map(|n| n.id.clone()).collect();
        for id in gone {
            self.step_notice(&id, NoticeEvent::Delivered);
        }
        for notice in listed {
            let id = notice.id.clone();
            self.step_notice(&id, NoticeEvent::Admitted(notice));
        }
        self.items = rows.iter().filter_map(parse_inbox_item).collect();
    }

    /// Applies one streamed `session.inbox.*` event.
    pub fn apply(&mut self, ty: &str, d: &Value) -> InboxChange {
        let id = d["inboxID"].as_str().unwrap_or("");
        let compaction = match ty {
            "session.inbox.enqueued" if d["item"]["type"] == "compaction" => Some(id.to_owned()),
            "session.inbox.cancelled" | "session.inbox.delivered" if self.compaction.as_deref() == Some(id) => None,
            _ => self.compaction.clone(),
        };
        let moved = compaction != self.compaction;
        self.compaction = compaction;
        let notices = self.notices.clone();
        match ty {
            "session.inbox.enqueued" => {
                if let Some(n) = parse_inbox_notice(d) {
                    let id = n.id.clone();
                    self.step_notice(&id, NoticeEvent::Admitted(n));
                }
            }
            "session.inbox.cancelled" => self.step_notice(id, NoticeEvent::Cancelled),
            "session.inbox.delivered" => self.step_notice(id, NoticeEvent::Delivered),
            _ => {}
        }
        let moved = moved || notices != self.notices;
        let changed = match ty {
            "session.inbox.enqueued" => match parse_inbox_item(d) {
                Some(item) => {
                    let same = self.items.contains(&item);
                    self.upsert(item);
                    !same
                }
                None => false,
            },
            "session.inbox.delivery.changed" => Delivery::parse(&d["delivery"]).is_some_and(|to| self.set_delivery(id, to)),
            "session.inbox.cancelled" => self.remove(id).is_some(),
            "session.inbox.delivered" => match self.remove(id) {
                Some(item) => return InboxChange::Delivered(item),
                None => false,
            },
            _ => false,
        } || moved;
        if changed { InboxChange::Changed } else { InboxChange::None }
    }
}
