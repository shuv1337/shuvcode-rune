//! The session data every window of one server shares, and the server's event stream. The
//! server's projects, sessions, transcripts, running sessions, stops, open requests and inboxes
//! are one copy for the process ([`ServerData`]), as Desktop's windows read one server's sync
//! store, so a session open in two windows loads once and updates in both. What each window
//! shows (tabs, page, panes, scroll, drafts, closed tabs) stays its own.
//!
//! The copy lives in the server's [`ServerPeer`] between turns. Rune runs one window's turn at a
//! time, and each turn starts and ends in `App::turn_scope`: [`enter`] moves the copy into the
//! window's own fields, [`leave`] moves it back, so code reads and writes plain fields with no
//! shared borrow (a turn that finds the copy out has a bug, and panics).
//!
//! The peer holds the server's one event stream. The first window to poll after events come
//! applies them to the copy once (`App::apply_shared`), and every window of the server, itself
//! included, then reacts to each in its own view (`App::react`), in order. A window that closes
//! or opens mid-stream loses nothing: the events wait in the peer, not in a window. A window
//! that changes the copy from what only it heard (its own reads' replies, a press of Stop)
//! marks it ([`touch`]), and the others draw it when its turn ends. The app's server runtime
//! (`servers::runtime`) holds each listed server's peer, so it lives as long as the server is
//! listed, with or without windows; a window works against one server at a time, and its halves
//! of the others keep what they read before joining (`stash`, `unstash`).

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use opencode_kit::client::{EventHub, Incoming};
use opencode_kit::model::{Inbox, Pending, Project, SessionInfo, Transcript};
use rune::Tracked;

use crate::events::Heard;
use crate::{App, Source};

/// One server's session data. Its fields are whole `Tracked` cells, traded with the window's
/// as they are, so a cell keeps its id and version from window to window: a region or cache
/// keyed on a version holds across turns until some window writes the cell.
#[derive(Default)]
pub struct ServerData {
    recent_page: Tracked<crate::recent_page::Page>,
    recent_reload: crate::recent_page::Reload,
    projects: Tracked<Vec<Project>>,
    sessions: Tracked<Vec<SessionInfo>>,
    transcripts: Tracked<HashMap<String, Transcript>>,
    transcript_edits: crate::events::TranscriptEdits,
    running: Tracked<HashSet<String>>,
    stops: Tracked<crate::stop::Stops>,
    requests: Tracked<HashSet<(String, String)>>,
    pending: Tracked<Vec<Pending>>,
    inboxes: Tracked<HashMap<String, Inbox>>,
    /// The prompts on their way to the server, from any of its windows (`send_failed`).
    unsent: Tracked<crate::send_failed::Log>,
    /// The index the palette and Home search the sessions in: one for all the server's windows.
    session_search: crate::switcher::search_index::SessionSearch,
}

impl ServerData {
    /// Trades places with the window's fields: no copies, no allocations, no writes.
    fn swap(&mut self, s: &mut App) {
        std::mem::swap(&mut self.recent_page, &mut s.recent_page);
        std::mem::swap(&mut self.recent_reload, &mut s.recent_reload);
        std::mem::swap(&mut self.projects, &mut s.projects);
        std::mem::swap(&mut self.sessions, &mut s.sessions);
        std::mem::swap(&mut self.transcripts, &mut s.transcripts);
        std::mem::swap(&mut self.transcript_edits, &mut s.transcript_edits);
        std::mem::swap(&mut self.running, &mut s.running);
        std::mem::swap(&mut self.stops, &mut s.stops);
        std::mem::swap(&mut self.requests, &mut s.requests);
        std::mem::swap(&mut self.pending, &mut s.pending);
        std::mem::swap(&mut self.inboxes, &mut s.inboxes);
        std::mem::swap(&mut self.unsent, &mut s.unsent);
        std::mem::swap(&mut self.session_search, &mut s.session_search);
    }

    /// The window's fields, which it no longer holds.
    fn take(s: &mut App) -> ServerData {
        let mut data = ServerData::default();
        data.swap(s);
        data
    }

    /// A window joins with what it read: what this copy lacks goes in (sessions and transcripts
    /// no other window loaded, its projects while none are listed).
    fn absorb(&mut self, mut from: ServerData) {
        if self.sessions.is_empty() {
            *self.recent_page = std::mem::take(&mut *from.recent_page);
            self.recent_reload = from.recent_reload;
        }
        if self.projects.is_empty() {
            *self.projects = std::mem::take(&mut *from.projects);
        }
        for info in from.sessions.drain(..) {
            if !self.sessions.iter().any(|x| x.id == info.id) {
                self.sessions.push(info);
            }
        }
        for (id, transcript) in std::mem::take(&mut *from.transcripts) {
            self.transcripts.entry(id).or_insert(transcript);
        }
        self.running.extend(std::mem::take(&mut *from.running));
        self.stops.active_sync.absorb(std::mem::take(&mut from.stops.active_sync));
        self.requests.extend(std::mem::take(&mut *from.requests));
        for p in std::mem::take(&mut *from.pending) {
            if !self.pending.iter().any(|x| x.id() == p.id()) {
                self.pending.push(p);
            }
        }
        for (id, inbox) in std::mem::take(&mut *from.inboxes) {
            self.inboxes.entry(id).or_insert(inbox);
        }
        self.unsent.absorb(std::mem::take(&mut *from.unsent));
    }
}

/// The peer's half of the copy's trade with the window whose turn it is. A turn trades `data`
/// with the window's fields in place, so no cell is made or dropped for it.
struct Vault {
    /// [`At::Home`]: the copy. [`At::Out`]: the window's own empty cells, which go back into its
    /// fields when its turn ends.
    data: ServerData,
    at: At,
}

/// Where the copy is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    /// In the peer, between turns.
    Home,
    /// In the fields of window `token`, whose turn it is.
    Out { token: u64 },
}

/// Whether a window has drawn the copy as it is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Current,
    /// Another window changed it since.
    Stale,
}

/// One window of the server.
struct Member {
    token: u64,
    wake: Option<rune::Waker>,
    /// Stream events applied to the copy that it hasn't reacted to yet, in order.
    heard: VecDeque<Rc<Heard>>,
    view: View,
}

/// The server's event stream, while windows hold it (`ServerPeer`'s lifecycle: it opens with
/// the first window to join and ends with the last to leave, when the peer drops).
struct Stream {
    hub: EventHub,
    events: Receiver<Incoming>,
}

/// One server's shared data and stream, held by its windows.
pub struct ServerPeer {
    vault: RefCell<Vault>,
    stream: Stream,
    members: RefCell<Vec<Member>>,
    /// The members' wakers, for the stream's thread to wake them all.
    wakers: Arc<Mutex<Vec<(u64, rune::Waker)>>>,
}

impl ServerPeer {
    /// Every user message id in this server's transcripts while its vault is at home (another
    /// server's window resting must not prune user-message metadata belonging to this server).
    pub fn user_message_ids(&self) -> Vec<String> {
        let vault = self.vault.borrow();
        if vault.at != At::Home {
            return Vec::new();
        }
        vault
            .data
            .transcripts
            .values()
            .flat_map(|t| t.messages.iter())
            .filter_map(|m| match m {
                opencode_kit::model::Message::User { id, .. } => Some(id.clone()),
                _ => None,
            })
            .chain(vault.data.inboxes.values().flat_map(|i| i.items.iter().map(|it| it.id.clone())))
            .collect()
    }
}

/// Whether a window's turn changed the copy from what only it heard.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Edit {
    #[default]
    Clean,
    Dirty,
}

/// A window's place in its server's peer.
#[derive(Default)]
pub struct Link {
    peer: Option<Rc<ServerPeer>>,
    /// This window among the members.
    token: u64,
    edit: Edit,
    /// What the window read of the server before it joined the peer, while it works against
    /// another server (`servers::runtime`).
    local: ServerData,
}

impl Link {
    /// User message ids in transcripts held locally before joining a peer (plus inbox items).
    pub(crate) fn local_user_message_ids(&self) -> Vec<String> {
        self.local
            .transcripts
            .values()
            .flat_map(|t| t.messages.iter())
            .filter_map(|m| match m {
                opencode_kit::model::Message::User { id, .. } => Some(id.clone()),
                _ => None,
            })
            .chain(self.local.inboxes.values().flat_map(|i| i.items.iter().map(|it| it.id.clone())))
            .collect()
    }

    /// Version stamp of this server's `projects`, `sessions`, and `recent_page` cells, whether
    /// held in a shared [`ServerPeer`] vault or stashed locally before joining.
    pub(crate) fn home_data_stamp(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let hash_data = |data: &ServerData| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (
                Tracked::id(&data.projects),
                Tracked::version(&data.projects),
                Tracked::id(&data.sessions),
                Tracked::version(&data.sessions),
                Tracked::id(&data.recent_page),
                Tracked::version(&data.recent_page),
            )
                .hash(&mut h);
            h.finish()
        };
        match &self.peer {
            Some(peer) => {
                let vault = peer.vault.borrow();
                if vault.at == At::Home { hash_data(&vault.data) } else { 0 }
            }
            None => hash_data(&self.local),
        }
    }

    /// Whether this server's known session list contains `sid`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn has_session(&self, sid: &str) -> bool {
        match &self.peer {
            Some(peer) => {
                let vault = peer.vault.borrow();
                vault.at == At::Home && vault.data.sessions.iter().any(|s| s.id == sid)
            }
            None => self.local.sessions.iter().any(|s| s.id == sid),
        }
    }
}

thread_local! {
    static TOKENS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn next_token() -> u64 {
    TOKENS.with(|t| {
        t.set(t.get() + 1);
        t.get()
    })
}

/// The window's half of a server it hasn't joined is put away: what it read goes with the half.
pub fn stash(s: &mut App) {
    if s.store.peer.is_none() {
        s.store.local = ServerData::take(s);
    }
}

/// The window's half of a server it hasn't joined comes back, with what it read.
pub fn unstash(s: &mut App) {
    if s.store.peer.is_none() {
        let mut local = std::mem::take(&mut s.store.local);
        local.swap(s);
    }
}

/// A new member of `peer` for a window's half that has read nothing of its own yet: the copy is
/// its from the start (a torn-out tab's window, a server the window gets a half of).
pub fn member(peer: Rc<ServerPeer>, wake: Option<rune::Waker>) -> Link {
    let token = next_token();
    if let Some(w) = &wake {
        peer.wakers.lock().unwrap_or_else(|e| e.into_inner()).push((token, w.clone()));
    }
    peer.members.borrow_mut().push(Member { token, wake, heard: VecDeque::new(), view: View::Current });
    Link { peer: Some(peer), token, edit: Edit::Clean, local: ServerData::default() }
}

/// `App::turn_scope`: the window's turn starts, with its server's copy in its fields.
pub fn enter(s: &mut App) {
    let Some(peer) = s.store.peer.clone() else { return };
    let token = s.store.token;
    let mut vault = peer.vault.borrow_mut();
    match vault.at {
        At::Home => {
            vault.data.swap(s);
            vault.at = At::Out { token };
        }
        At::Out { token: other } if other == token => {}
        At::Out { token: other } => panic!("window {token}'s turn found the server data in window {other}'s"),
    }
}

/// `App::turn_scope`: the turn ends; the copy goes back, and if the turn changed it from what
/// only it heard, the server's other windows draw it.
pub fn leave(s: &mut App) {
    let Some(peer) = s.store.peer.clone() else { return };
    let token = s.store.token;
    let mut vault = peer.vault.borrow_mut();
    match vault.at {
        At::Out { token: t } if t == token => {
            vault.data.swap(s);
            vault.at = At::Home;
        }
        At::Home => {}
        At::Out { token: other } => panic!("window {token}'s turn ended with the server data in window {other}'s"),
    }
    drop(vault);
    if std::mem::take(&mut s.store.edit) == Edit::Dirty {
        stale_others(&peer, s.store.token);
    }
}

fn stale_others(peer: &ServerPeer, token: u64) {
    let wake: Vec<_> = peer.members.borrow_mut().iter_mut().filter(|m| m.token != token).filter_map(|m| {
        m.view = View::Stale;
        m.wake.clone()
    }).collect();
    wake.iter().for_each(|w| w());
}

/// The window changed the copy from what only it heard (a reply, a press): the others draw it.
pub fn touch(s: &mut App) {
    if s.store.peer.is_some() {
        s.store.edit = Edit::Dirty;
    }
}

/// `poll`: another window changed the copy since this one drew it.
pub fn behind(s: &mut App) -> bool {
    let Some(peer) = &s.store.peer else { return false };
    let token = s.store.token;
    peer.members.borrow_mut().iter_mut().find(|m| m.token == token).is_some_and(|m| std::mem::replace(&mut m.view, View::Current) == View::Stale)
}

/// The window, connected to its server, joins that server's peer, or starts it: what it read
/// goes into the copy, and the copy is in its fields for the rest of the turn.
pub fn join(s: &mut App, wake: Option<rune::Waker>) {
    let Source::Live { server, .. } = &*s.source else { return };
    let server = server.clone();
    let base = server.base.clone();
    if s.store.peer.as_ref().is_some_and(|p| p.stream.hub.base() == base) {
        return;
    }
    leave_server(s);
    let token = next_token();
    let key = crate::servers::runtime::active(s).clone();
    let found = crate::servers::runtime::peer(&key).filter(|p| p.stream.hub.base() == base);
    let peer = match found {
        Some(peer) => {
            let mut vault = peer.vault.borrow_mut();
            assert!(vault.at == At::Home, "a window joined while another's turn held the server data");
            vault.data.absorb(ServerData::take(s));
            vault.data.swap(s);
            vault.at = At::Out { token };
            drop(vault);
            peer
        }
        None => {
            let wakers: Arc<Mutex<Vec<(u64, rune::Waker)>>> = Arc::default();
            let all = wakers.clone();
            let hub = EventHub::start(server.clone(), crate::reconnect::STREAM_IDLE);
            let events = hub.subscribe(Arc::new(move || all.lock().unwrap_or_else(|e| e.into_inner()).iter().for_each(|(_, w)| w())));
            let vault = Vault { data: ServerData::default(), at: At::Out { token } };
            let peer = Rc::new(ServerPeer { vault: RefCell::new(vault), stream: Stream { hub, events }, members: RefCell::default(), wakers });
            // The runtime holds it while the server is listed, whatever windows come and go.
            crate::servers::runtime::keep_peer(&key, &peer);
            peer
        }
    };
    if let Some(w) = &wake {
        peer.wakers.lock().unwrap_or_else(|e| e.into_inner()).push((token, w.clone()));
    }
    peer.members.borrow_mut().push(Member { token, wake, heard: VecDeque::new(), view: View::Current });
    **s.store = Link { peer: Some(peer), token, edit: Edit::Dirty, local: ServerData::default() };
}

/// What a window has read of its server and can show without it: its projects, sessions and
/// loaded transcripts, copied out of the copy.
pub struct Reading {
    recent_page: crate::recent_page::Page,
    projects: Vec<Project>,
    sessions: Vec<SessionInfo>,
    transcripts: HashMap<String, Transcript>,
}

/// The window's reading of its server, before its half is replaced (the server signed us out,
/// moved, or signed back in): what it read stays on screen instead of going with the copy.
pub fn reading(s: &App) -> Reading {
    Reading { recent_page: (*s.recent_page).clone(), projects: (*s.projects).clone(), sessions: (*s.sessions).clone(), transcripts: (*s.transcripts).clone() }
}

/// Puts `reading` into a fresh half's empty fields.
pub fn keep_reading(s: &mut App, reading: Reading) {
    if s.projects.is_empty() {
        *s.projects = reading.projects;
    }
    if s.sessions.is_empty() {
        *s.recent_page = reading.recent_page;
        *s.sessions = reading.sessions;
    }
    for (id, t) in reading.transcripts {
        s.transcripts.entry(id).or_insert(t);
    }
}

/// The window leaves its server's peer (it closes, moves to another server, or loses it): it
/// keeps nothing of the copy, and the other windows keep it all. The last to leave ends the
/// peer, stream and data.
pub fn leave_server(s: &mut App) {
    let Some(peer) = s.store.peer.take() else { return };
    let token = s.store.token;
    let mut vault = peer.vault.borrow_mut();
    if vault.at == (At::Out { token }) {
        vault.data.swap(s);
        vault.at = At::Home;
        // Its writes still out answer no window now: a receipt check settles them.
        if vault.data.unsent.release(token) {
            s.store.edit = Edit::Dirty;
        }
    }
    drop(vault);
    peer.members.borrow_mut().retain(|m| m.token != token);
    peer.wakers.lock().unwrap_or_else(|e| e.into_inner()).retain(|(t, _)| *t != token);
    if std::mem::take(&mut s.store.edit) == Edit::Dirty {
        stale_others(&peer, token);
    }
}

/// `setup`, for an adopted window: the stream wakes it from now on.
pub fn set_waker(s: &App, wake: rune::Waker) {
    let Some(peer) = &s.store.peer else { return };
    let token = s.store.token;
    if let Some(m) = peer.members.borrow_mut().iter_mut().find(|m| m.token == token) {
        m.wake = Some(wake.clone());
    }
    let mut wakers = peer.wakers.lock().unwrap_or_else(|e| e.into_inner());
    wakers.retain(|(t, _)| *t != token);
    wakers.push((token, wake));
}

/// A waker for work done for the copy (the shared session index): it wakes every window of the
/// server, so whichever has the next turn lands it. The window's own outside a peer.
pub fn waker(s: &App) -> Option<rune::Waker> {
    let Some(peer) = &s.store.peer else { return (*s.waker).clone() };
    let all = peer.wakers.clone();
    Some(Arc::new(move || all.lock().unwrap_or_else(|e| e.into_inner()).iter().for_each(|(_, w)| w())))
}

/// This window among its server's windows (0 before it joins one): whose writer a prompt went
/// out through (`send_failed::Phase::Sending`).
pub fn token(s: &App) -> u64 {
    s.store.token
}

/// The window's server's stream events of type `ty` only, woken by `wake` (`EventHub::subscribe_to`),
/// with that server's base: None before the window joins one.
#[cfg(not(target_arch = "wasm32"))]
pub fn subscribe_to(s: &App, ty: &'static str, wake: rune::Waker) -> Option<(String, Receiver<Incoming>)> {
    let hub = &s.store.peer.as_ref()?.stream.hub;
    Some((hub.base().to_owned(), hub.subscribe_to(ty, wake)))
}

/// The base of the window's server's stream, once it joined one.
#[cfg(not(target_arch = "wasm32"))]
pub fn base(s: &App) -> Option<&str> {
    Some(s.store.peer.as_ref()?.stream.hub.base())
}

/// Whether the window holds a peer (it joined, or was adopted into one).
pub fn joined(s: &App) -> bool {
    s.store.peer.is_some()
}

/// Runs `f` on `s` in a turn of its own, for work outside the event loop's turns (a window's
/// first state, built before its first turn).
pub fn in_turn(s: &mut App, f: impl FnOnce(&mut App)) {
    crate::servers::runtime::enter(s);
    f(s);
    crate::servers::runtime::leave(s);
}

/// `poll`: the stream's new events, applied to the copy once by whichever window polls first,
/// then what this window hasn't reacted to yet, in order.
pub fn heard(s: &mut App) -> Vec<Rc<Heard>> {
    let Some(peer) = s.store.peer.clone() else { return Vec::new() };
    let fresh: Vec<Incoming> = peer.stream.events.try_iter().collect();
    if !fresh.is_empty() {
        let applied: Vec<Rc<Heard>> = fresh.into_iter().map(|m| Rc::new(s.apply_shared(m))).collect();
        let token = s.store.token;
        let wake: Vec<_> = peer.members.borrow_mut().iter_mut().filter_map(|m| {
            m.heard.extend(applied.iter().cloned());
            m.wake.clone().filter(|_| m.token != token)
        }).collect();
        wake.iter().for_each(|w| w());
    }
    let token = s.store.token;
    peer.members.borrow_mut().iter_mut().find(|m| m.token == token).map(|m| m.heard.drain(..).collect()).unwrap_or_default()
}
